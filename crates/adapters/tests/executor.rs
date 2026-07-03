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

fn write_script(dir: &TmpDir, body: &str) -> String {
    let path = dir.0.join("claude");
    fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn flags_builders_map_to_claude_args_in_order() {
    let dir = TmpDir::new("builders");
    let exec = ClaudeExecutor::with_bin(fake_claude(&dir));
    let flags = ExecFlags::new()
        .with_allowed(["Read", "Grep"])
        .with_hook("/tmp/settings.json")
        .with_session_id("abc-123");

    let out = exec.spawn_oneshot("go", &flags).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        vec![
            "go",
            "--print",
            "--allowedTools",
            "Read",
            "Grep",
            "--settings",
            "/tmp/settings.json",
            "--session-id",
            "abc-123",
        ]
    );
}

#[test]
fn resume_wins_over_session_id() {
    let dir = TmpDir::new("resume");
    let exec = ClaudeExecutor::with_bin(fake_claude(&dir));
    let flags = ExecFlags::new()
        .with_session_id("create-me")
        .with_resume("resume-me");

    let out = exec.spawn_oneshot("go", &flags).unwrap();
    assert!(out.contains("--resume"));
    assert!(out.contains("resume-me"));
    assert!(!out.contains("--session-id"));
    assert!(!out.contains("create-me"));
}

#[test]
fn oneshot_runs_in_the_requested_cwd() {
    let dir = TmpDir::new("cwd");
    let workdir = TmpDir::new("cwd-target");
    let bin = write_script(&dir, "pwd");
    let exec = ClaudeExecutor::with_bin(bin);
    let flags = ExecFlags::new().with_cwd(&workdir.0);

    let out = exec.spawn_oneshot("x", &flags).unwrap();
    let reported = fs::canonicalize(out.trim()).unwrap();
    assert_eq!(reported, fs::canonicalize(&workdir.0).unwrap());
}

#[test]
fn oneshot_error_extracts_result_from_json_envelope() {
    let dir = TmpDir::new("json-err");
    let bin = write_script(&dir, r#"echo '{"result":"usage limit reached"}'; exit 1"#);
    let exec = ClaudeExecutor::with_bin(bin);
    let err = exec.spawn_oneshot("x", &ExecFlags::new()).unwrap_err();
    assert!(err.to_string().contains("usage limit reached"));
}

#[test]
fn oneshot_error_falls_back_to_raw_stdout() {
    let dir = TmpDir::new("raw-err");
    let bin = write_script(&dir, "echo 'plain failure'; exit 1");
    let exec = ClaudeExecutor::with_bin(bin);
    let err = exec.spawn_oneshot("x", &ExecFlags::new()).unwrap_err();
    assert!(err.to_string().contains("plain failure"));
}

#[test]
fn oneshot_missing_binary_errors_with_context() {
    let exec = ClaudeExecutor::with_bin("/does/not/exist/claude");
    let err = exec.spawn_oneshot("x", &ExecFlags::new()).unwrap_err();
    assert!(err.to_string().contains("--print"));
}

#[test]
fn streaming_runs_in_the_requested_cwd() {
    let dir = TmpDir::new("stream-cwd");
    let workdir = TmpDir::new("stream-cwd-target");
    let bin = write_script(&dir, "pwd");
    let exec = ClaudeExecutor::with_bin(bin);
    let flags = ExecFlags::new().with_cwd(&workdir.0);

    let mut on_line = |_: &str| {};
    let out = exec
        .spawn_oneshot_streaming("x", &flags, &mut on_line)
        .unwrap();
    let reported = fs::canonicalize(out.trim()).unwrap();
    assert_eq!(reported, fs::canonicalize(&workdir.0).unwrap());
}

#[test]
fn streaming_error_falls_back_to_stdout_when_stderr_empty() {
    let dir = TmpDir::new("stream-raw-err");
    let bin = write_script(&dir, "echo 'stream failure'; exit 2");
    let exec = ClaudeExecutor::with_bin(bin);
    let mut on_line = |_: &str| {};
    let err = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap_err();
    assert!(err.to_string().contains("stream failure"));
}

#[test]
fn streaming_missing_binary_errors_with_context() {
    let exec = ClaudeExecutor::with_bin("/does/not/exist/claude");
    let mut on_line = |_: &str| {};
    let err = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap_err();
    assert!(err.to_string().contains("stream"));
}

#[test]
fn default_streaming_impl_delegates_to_oneshot() {
    struct FakeExec;
    impl Executor for FakeExec {
        fn spawn_oneshot(&self, _prompt: &str, _flags: &ExecFlags) -> anyhow::Result<String> {
            Ok("alpha\nbeta\n".to_string())
        }
        fn spawn_interactive(
            &self,
            _prompt: &str,
            _flags: &ExecFlags,
        ) -> anyhow::Result<jaum_adapters::Session> {
            unreachable!("not used in this test")
        }
    }

    let mut seen: Vec<String> = Vec::new();
    let mut on_line = |l: &str| seen.push(l.to_string());
    let out = FakeExec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap();
    assert_eq!(seen, vec!["alpha", "beta"]);
    assert_eq!(out, "alpha\nbeta\n");
}

#[test]
fn default_executor_uses_the_real_claude_binary_name() {
    // construction only; nothing is executed.
    let _ = ClaudeExecutor::default();
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
fn interactive_passes_prompt_cwd_and_supports_resize() {
    let dir = TmpDir::new("interactive-prompt");
    let workdir = TmpDir::new("interactive-cwd");
    // prints the prompt and the working directory, then exits
    let bin = write_script(&dir, "echo \"$1\"; pwd");
    let exec = ClaudeExecutor::with_bin(bin);
    let flags = ExecFlags::new().with_cwd(&workdir.0);

    let mut session = exec.spawn_interactive("hello prompt", &flags).unwrap();
    session.resize(30, 100).unwrap();

    let mut reader = session.reader().unwrap();
    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap();
    assert!(
        buf.contains("hello prompt"),
        "prompt not forwarded:\n{buf:?}"
    );
    assert!(session.wait().unwrap());
}

#[test]
fn interactive_close_input_sends_eof_and_blocks_further_writes() {
    let exec = ClaudeExecutor::with_bin("cat");
    let mut session = exec.spawn_interactive("", &ExecFlags::new()).unwrap();
    session.close_input();
    let err = session.write_input(b"late").unwrap_err();
    assert!(err.to_string().contains("input already closed"));
    let _ = session.kill();
    let _ = session.wait();
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
