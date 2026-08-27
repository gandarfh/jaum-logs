//! End-to-end tests spawning the real `jaum` binary. Every process gets an
//! isolated HOME (so `~/jaum` never touches the user's), and anything that
//! reaches the TUI runs in its own session (setsid) so it can never grab the
//! developer's terminal: without a controlling tty the client fails fast.

use std::fs;
use std::io::{BufReader, Read, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[path = "../src/config.rs"]
#[allow(dead_code)]
mod config;
#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use config::{Config, Project};
use protocol::{ClientMsg, write_msg};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn jaum_bin() -> &'static str {
    env!("CARGO_BIN_EXE_jaum")
}

/// Isolated HOME for one test; removed on drop.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-bin-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(p.join("jaum")).unwrap();
        Home(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn sock(&self) -> PathBuf {
        self.0.join("jaum/daemon.sock")
    }

    /// Registers a project with a small backlog and returns its root.
    fn with_project(&self) -> PathBuf {
        let root = self.0.join("proj");
        let backlog = self.0.join("jaum/proj/backlog");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&backlog).unwrap();
        fs::create_dir_all(self.0.join("jaum/proj/docs")).unwrap();
        fs::write(
            backlog.join("TASK-001.md"),
            "---\nid: TASK-001\ntype: impl\nstatus: backlog\n---\n\n## Objective\nx\n",
        )
        .unwrap();
        let cfg = Config {
            projects: vec![Project {
                name: "proj".into(),
                root: root.clone(),
                backlog,
                docs: self.0.join("jaum/proj/docs"),
                work_dir: self.0.join("jaum/proj/work"),
                repos: Vec::new(),
            }],
        };
        cfg.save_to(&self.0.join("jaum/config.toml")).unwrap();
        root
    }

    /// Overwrites the project's configured repos (for --repo resolution tests).
    fn set_repos(&self, slugs: &[&str]) {
        let cfg_path = self.0.join("jaum/config.toml");
        let mut cfg = Config::load_from(&cfg_path).unwrap();
        cfg.projects[0].repos = slugs
            .iter()
            .map(|s| config::RepoMap {
                slug: s.to_string(),
                path: self.0.join(format!("repo-{s}")),
            })
            .collect();
        cfg.save_to(&cfg_path).unwrap();
    }

    /// Backlog dir of the project created by `with_project()`.
    fn backlog_dir(&self) -> PathBuf {
        self.0.join("jaum/proj/backlog")
    }

    fn jaum(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(jaum_bin());
        cmd.args(args)
            .env("HOME", self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Best-effort daemon shutdown so a failing test never leaks a process.
    fn stop_daemon(&self) {
        if let Ok(mut s) = UnixStream::connect(self.sock()) {
            let _ = write_msg(&mut s, &ClientMsg::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.sock().exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.stop_daemon();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs in a fresh session: no controlling terminal, so any code path that
/// needs a real tty fails instead of touching the terminal running the tests.
fn run_detached(mut cmd: Command) -> Output {
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    wait_with_timeout(cmd.spawn().unwrap(), Duration::from_secs(30))
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    panic!("process did not exit within {timeout:?}");
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn wait_for_socket(sock: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if UnixStream::connect(sock).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// --- init --------------------------------------------------------------------

#[test]
fn init_detects_repos_and_registers_project() {
    let home = Home::new("init");
    let root = home.path().join("widget");
    fs::create_dir_all(root.join(".git")).unwrap();

    let out = home.jaum(&["init"]).current_dir(&root).output().unwrap();
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let text = stdout_of(&out);
    assert!(text.contains("project 'widget' registered"));
    assert!(text.contains("repo: widget ->"), "stdout: {text}");
    assert!(home.path().join("jaum/config.toml").exists());
    assert!(home.path().join("jaum/widget/conventions.md").exists());
}

#[test]
fn init_reports_when_no_repo_is_detected() {
    let home = Home::new("init-empty");
    let root = home.path().join("bare");
    fs::create_dir_all(&root).unwrap();

    let out = home.jaum(&["init"]).current_dir(&root).output().unwrap();
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).contains("(none detected"));
}

// --- list --------------------------------------------------------------------

#[test]
fn list_requires_a_registered_project() {
    let home = Home::new("list-empty");
    let out = home.jaum(&["list"]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("no project registered"));
}

#[test]
fn list_prints_backlog_from_project_cwd_and_elsewhere() {
    let home = Home::new("list");
    let root = home.with_project();

    // from the project root (cwd match)
    let out = home.jaum(&["list"]).current_dir(&root).output().unwrap();
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).contains("TASK-001"));

    // from an unrelated cwd (falls back to the first project)
    let elsewhere = home.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = home
        .jaum(&["list"])
        .current_dir(&elsewhere)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("TASK-001"));
}

// --- task new ------------------------------------------------------------

fn task_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("TASK-"))
        .collect();
    names.sort();
    names
}

#[test]
fn task_new_creates_task_with_objective_criteria_rfcs_and_adrs() {
    let home = Home::new("task-new-ok");
    let root = home.with_project();

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "do the thing",
            "--criteria",
            "first thing works",
            "--criteria",
            "second thing works",
            "--rfc",
            "rfc-a",
            "--rfc",
            "rfc-b",
            "--adr",
            "adr-a",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert_eq!(stdout.lines().next(), Some("TASK-002"));

    let content = fs::read_to_string(home.backlog_dir().join("TASK-002.md")).unwrap();
    assert!(content.contains("type: impl"));
    assert!(content.contains("rfc-a"));
    assert!(content.contains("rfc-b"));
    assert!(content.contains("adr-a"));
    assert!(content.contains("## Objective\n\ndo the thing"));
    assert!(content.contains("- [ ] first thing works"));
    assert!(content.contains("- [ ] second thing works"));
    // order preserved
    let first_pos = content.find("first thing works").unwrap();
    let second_pos = content.find("second thing works").unwrap();
    assert!(first_pos < second_pos);
}

#[test]
fn task_new_rejects_invalid_type_and_writes_nothing() {
    let home = Home::new("task-new-bad-type");
    let root = home.with_project();

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "refactor",
            "--objective",
            "x",
            "--criteria",
            "y",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("'refactor' is not a valid task type"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("impl, spike"), "stderr: {stderr}");
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_requires_objective() {
    let home = Home::new("task-new-no-objective");
    let root = home.with_project();

    let out = home
        .jaum(&["task", "new", "--type", "impl", "--criteria", "y"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("missing required --objective"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_rejects_blank_objective() {
    let home = Home::new("task-new-blank-objective");
    let root = home.with_project();

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "   ",
            "--criteria",
            "y",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("--objective must not be blank"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_requires_at_least_one_criteria() {
    let home = Home::new("task-new-no-criteria");
    let root = home.with_project();

    let out = home
        .jaum(&["task", "new", "--type", "impl", "--objective", "x"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("at least one --criteria is required"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_rejects_blank_criteria_value() {
    let home = Home::new("task-new-blank-criteria");
    let root = home.with_project();

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "   ",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("--criteria must not be blank"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_rejects_unknown_flag() {
    let home = Home::new("task-new-unknown-flag");
    let root = home.with_project();

    let out = home
        .jaum(&["task", "new", "--bogus", "x"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("unknown flag '--bogus'"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_rejects_repo_without_branch() {
    let home = Home::new("task-new-repo-no-branch");
    let root = home.with_project();

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "y",
            "--repo",
            "org/app",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("--repo requires --branch"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_links_repo_when_single_repo_configured() {
    let home = Home::new("task-new-single-repo");
    let root = home.with_project();
    home.set_repos(&["org/app"]);

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "y",
            "--branch",
            "feat/x",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("linked: org/app@feat/x"),
        "stdout: {stdout}"
    );

    let content = fs::read_to_string(home.backlog_dir().join("TASK-002.md")).unwrap();
    assert!(content.contains("org/app"));
    assert!(content.contains("feat/x"));
}

#[test]
fn task_new_requires_explicit_repo_when_multiple_configured() {
    let home = Home::new("task-new-multi-repo");
    let root = home.with_project();
    home.set_repos(&["org/a", "org/b"]);

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "y",
            "--branch",
            "feat/x",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("multiple repos are configured"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("org/a") && stderr.contains("org/b"),
        "stderr: {stderr}"
    );
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_rejects_unknown_repo_slug() {
    let home = Home::new("task-new-unknown-repo");
    let root = home.with_project();
    home.set_repos(&["org/app"]);

    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "y",
            "--repo",
            "org/other",
            "--branch",
            "feat/x",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("unknown --repo 'org/other'"));
    assert_eq!(task_files(&home.backlog_dir()), vec!["TASK-001.md"]);
}

#[test]
fn task_new_requires_a_registered_project() {
    let home = Home::new("task-new-no-project");
    let out = home
        .jaum(&[
            "task",
            "new",
            "--type",
            "impl",
            "--objective",
            "x",
            "--criteria",
            "y",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("no project registered"));
}

// --- shutdown / daemon -------------------------------------------------------

#[test]
fn shutdown_without_daemon_says_so() {
    let home = Home::new("shutdown-none");
    let out = home.jaum(&["shutdown"]).output().unwrap();
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("no daemon running"));
}

#[test]
fn daemon_serves_until_shutdown() {
    let home = Home::new("daemon");
    home.with_project();

    let child = home.jaum(&["--daemon", "0"]).spawn().unwrap();
    assert!(wait_for_socket(&home.sock()), "daemon never bound");

    let out = home.jaum(&["shutdown"]).output().unwrap();
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("daemon stopped"));

    let out = wait_with_timeout(child, Duration::from_secs(10));
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(!home.sock().exists());
}

#[test]
fn daemon_rejects_invalid_project_index() {
    let home = Home::new("daemon-bad-idx");
    home.with_project();
    let out = home.jaum(&["--daemon", "7"]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("invalid project index"));
}

// --- attach ------------------------------------------------------------------

#[test]
fn attach_spawns_daemon_then_fails_without_tty() {
    let home = Home::new("attach");
    home.with_project();

    let out = run_detached(home.jaum(&[]));
    assert!(!out.status.success(), "attach must fail with no tty");
    // the daemon it spawned must survive the failed client
    assert!(
        UnixStream::connect(home.sock()).is_ok(),
        "daemon should be running; stderr: {}",
        stderr_of(&out)
    );

    let out = home.jaum(&["shutdown"]).output().unwrap();
    assert!(stdout_of(&out).contains("daemon stopped"));
}

#[test]
fn attach_reports_daemon_that_never_comes_up() {
    let home = Home::new("attach-dead");
    home.with_project();
    // a directory at the socket path: the spawned daemon cannot bind and dies
    fs::create_dir_all(home.sock()).unwrap();

    let out = run_detached(home.jaum(&[]));
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("did not come up"),
        "stderr: {}",
        stderr_of(&out)
    );
    fs::remove_dir_all(home.sock()).unwrap();
}

#[test]
fn restart_replaces_running_daemon() {
    let home = Home::new("restart");
    home.with_project();

    let old = home.jaum(&["--daemon", "0"]).spawn().unwrap();
    assert!(wait_for_socket(&home.sock()), "daemon never bound");

    // restart: stops the old daemon, spawns a new one, then the client attach
    // fails (no tty) — the fresh daemon must stay up.
    let out = run_detached(home.jaum(&["restart"]));
    assert!(!out.status.success(), "attach must fail with no tty");
    let old_out = wait_with_timeout(old, Duration::from_secs(10));
    assert!(old_out.status.success(), "old daemon should exit cleanly");
    assert!(
        wait_for_socket(&home.sock()),
        "new daemon should be running; stderr: {}",
        stderr_of(&out)
    );

    let out = home.jaum(&["shutdown"]).output().unwrap();
    assert!(stdout_of(&out).contains("daemon stopped"));
}

// --- local -------------------------------------------------------------------

#[test]
fn local_tui_fails_without_tty() {
    let home = Home::new("local");
    home.with_project();
    let out = run_detached(home.jaum(&["--local"]));
    assert!(!out.status.success());
}

// --- full client session over a pty ------------------------------------------

/// Minimal but complete snapshot for the fake daemon.
fn sample_snapshot() -> protocol::DomainSnapshot {
    use protocol::*;
    DomainSnapshot {
        project: "proj".into(),
        projects: vec![ProjectRef {
            name: "proj".into(),
            backlog: "/tmp/backlog".into(),
        }],
        tab: TabId::Board,
        board: BoardView {
            tasks: vec![TaskView {
                id: "TASK-001".into(),
                task_type: TaskTypeId::Impl,
                status: StatusId::Backlog,
                rfcs: vec![],
                adrs: vec![],
                prs: vec![],
                deferred: vec![],
                constraints: vec![],
                body: "## Objective\nx\n".into(),
                live_session: false,
            }],
            selected: 0,
            project_selected: false,
            focus: FocusId::Tasks,
            cards: vec![],
            card_selected: 0,
            chat_fullscreen: false,
            setup_needed: false,
            setup_live: false,
            detail_open: false,
            detail_scroll: 0,
            overlaps: vec![],
        },
        docs: DocsView {
            dir: "/tmp/docs".into(),
            list: vec![],
            selected: 0,
            preview: String::new(),
            doc_open: false,
            scroll: 0,
        },
        picker: None,
        input: None,
        job: None,
        job_overlay: false,
        toast: None,
    }
}

/// Drives a complete attach: a fake daemon on the socket answers the
/// handshake, streams a snapshot, requests the editor and finally detaches,
/// while the client runs on a real pty allocated by `script(1)`.
#[test]
fn attach_runs_full_client_session_over_pty() {
    use protocol::{PROTOCOL_VERSION, ServerMsg, read_msg};
    use std::os::unix::net::UnixListener;

    let home = Home::new("pty");
    home.with_project();
    let marker = home.path().join("editor-ran");
    let editor = home.path().join("stub-editor");
    fs::write(
        &editor,
        format!("#!/bin/sh\necho \"$1\" > {}\n", marker.display()),
    )
    .unwrap();
    let mut perms = fs::metadata(&editor).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&editor, perms).unwrap();

    // fake daemon: bound before the client starts so attach() skips the spawn.
    let listener = UnixListener::bind(home.sock()).unwrap();

    let mut child = {
        let mut cmd = Command::new("/usr/bin/script");
        if cfg!(target_os = "linux") {
            // util-linux: the command goes through -c, the typescript file is
            // positional; -e forwards the child's exit status like BSD does.
            cmd.arg("-qe").arg("-c").arg(jaum_bin()).arg("/dev/null");
        } else {
            // BSD/macOS: typescript file first, then the command argv.
            cmd.arg("-q").arg("/dev/null").arg(jaum_bin());
        }
        cmd.env("HOME", home.path())
            .env("TERM", "xterm-256color")
            .env("EDITOR", &editor)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };

    // drain the pty output so the client never blocks on a full pipe
    let mut child_stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = child_stdout.read_to_end(&mut sink);
    });
    let mut stdin = child.stdin.take().unwrap();

    // accept until the real client shows up (attach() probes with a bare connect
    // and drops it; on such dead sockets setsockopt can fail — skip them).
    // Bounded: if the client dies before connecting, fail instead of hanging.
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut conn, mut reader) = loop {
        let conn = match listener.accept() {
            Ok((conn, _)) => conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("client never connected to the fake daemon");
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => panic!("accept failed: {e}"),
        };
        // the accepted socket may inherit non-blocking from the listener
        conn.set_nonblocking(false).unwrap();
        if conn
            .set_read_timeout(Some(Duration::from_secs(15)))
            .is_err()
        {
            continue;
        }
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        match read_msg::<_, ClientMsg>(&mut reader) {
            Ok(Some(ClientMsg::Hello {
                protocol_version, ..
            })) => {
                assert_eq!(protocol_version, PROTOCOL_VERSION, "client version");
                break (conn, reader);
            }
            _ => continue, // is_running probe: connect + drop
        }
    };

    // handshake answer + first snapshot so the client paints something real
    write_msg(
        &mut conn,
        &ServerMsg::Welcome {
            protocol_version: PROTOCOL_VERSION,
            device_id: 0,
        },
    )
    .unwrap();
    write_msg(&mut conn, &ServerMsg::Snapshot(Box::new(sample_snapshot()))).unwrap();

    // editor round-trip: client suspends, runs $EDITOR, answers EditorDone
    write_msg(
        &mut conn,
        &ServerMsg::RunEditor {
            path: "/tmp/jaum-pty-conv.md".into(),
        },
    )
    .unwrap();
    let mut got_editor_done = false;
    for _ in 0..10 {
        match read_msg::<_, ClientMsg>(&mut reader).unwrap() {
            Some(ClientMsg::EditorDone) => {
                got_editor_done = true;
                break;
            }
            Some(_) => continue, // pings keep flowing
            None => break,
        }
    }
    assert!(got_editor_done, "client never reported EditorDone");
    assert_eq!(
        fs::read_to_string(&marker).unwrap().trim(),
        "/tmp/jaum-pty-conv.md",
        "stub editor should have received the path"
    );

    // keystroke through the pty reaches the daemon as a domain intent
    stdin.write_all(b"j").unwrap();
    stdin.flush().unwrap();
    let mut got_intent = false;
    for _ in 0..10 {
        match read_msg::<_, ClientMsg>(&mut reader).unwrap() {
            Some(ClientMsg::Intent(protocol::Intent::SelectNext)) => {
                got_intent = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(got_intent, "keystroke never arrived as an intent");

    // a second snapshot keeps the redraw path warm, then detach ends the session
    write_msg(&mut conn, &ServerMsg::Snapshot(Box::new(sample_snapshot()))).unwrap();
    write_msg(&mut conn, &ServerMsg::Detach).unwrap();

    let out = wait_with_timeout(child, Duration::from_secs(15));
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
}
