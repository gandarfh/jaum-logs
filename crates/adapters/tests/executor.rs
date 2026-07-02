use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-exec-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fake `claude` that prints each argv on its own line — lets the test assert
/// exactly how the flags were assembled, without network.
fn fake_claude(dir: &TmpDir) -> String {
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\"\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn oneshot_builds_print_prompt_and_flags() {
    let dir = TmpDir::new("oneshot");
    let exec = ClaudeExecutor::with_bin(fake_claude(&dir));
    let flags = ExecFlags::new()
        .with_disallowed(["Edit", "Bash(git merge)"])
        .with_append_system_prompt("do not merge")
        .with_model("opus");

    let out = exec.spawn_oneshot("do the task", &flags).unwrap();

    // positional prompt comes before --print
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "do the task");
    assert_eq!(lines[1], "--print");
    assert!(out.contains("--disallowedTools"));
    assert!(out.contains("Bash(git merge)")); // single arg, parentheses preserved
    assert!(out.contains("--append-system-prompt"));
    assert!(out.contains("do not merge"));
    assert!(out.contains("--model"));
    assert!(out.contains("opus"));
}

#[test]
fn oneshot_propagates_nonzero_exit_error() {
    let dir = TmpDir::new("oneshot-err");
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\necho 'boom' >&2\nexit 3\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let err = exec.spawn_oneshot("x", &ExecFlags::new()).unwrap_err();
    assert!(err.to_string().contains("boom"));
}

#[test]
fn streaming_forwards_each_line_and_returns_all() {
    let dir = TmpDir::new("stream");
    let path = dir.0.join("claude");
    // prints 3 "event" lines to stdout
    fs::write(
        &path,
        "#!/usr/bin/env bash\nprintf 'one\\ntwo\\nthree\\n'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let mut seen: Vec<String> = Vec::new();
    let mut on_line = |l: &str| seen.push(l.to_string());
    let out = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap();

    assert_eq!(seen, vec!["one", "two", "three"]);
    assert_eq!(out, "one\ntwo\nthree\n");
}

#[test]
fn streaming_propagates_nonzero_exit_error() {
    let dir = TmpDir::new("stream-err");
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\necho 'kaboom' >&2\nexit 4\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let mut on_line = |_: &str| {};
    let err = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap_err();
    assert!(err.to_string().contains("kaboom"));
}

#[test]
fn interactive_roundtrip_via_pty() {
    // uses `cat` as the session: echoes what it receives.
    let exec = ClaudeExecutor::with_bin("cat");
    let mut session = exec.spawn_interactive("", &ExecFlags::new()).unwrap();

    let mut reader = session.reader().unwrap();
    session.write_line("ping").unwrap();
    session.write_input(&[0x04]).unwrap(); // Ctrl-D: EOF -> cat exits

    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap(); // blocks until EOF
    assert!(buf.contains("ping"), "PTY did not echo the input:\n{buf:?}");
    assert!(session.wait().unwrap(), "cat should exit successfully");
}

#[test]
fn interactive_kill_terminates_session() {
    // `sleep 5` via extra args; we kill it before it finishes.
    let exec = ClaudeExecutor::with_bin("sleep");
    let flags = ExecFlags {
        extra: vec!["5".to_string()],
        ..Default::default()
    };
    let mut session = exec.spawn_interactive("", &flags).unwrap();

    assert!(
        session.try_wait().unwrap().is_none(),
        "should still be running"
    );
    session.kill().unwrap();
    // after kill, wait returns (unsuccessful since it was killed)
    let success = session.wait().unwrap();
    assert!(!success, "a killed session should not report success");
}
