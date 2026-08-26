//! Behavior tests for the daemon: intent application, snapshot coalescing,
//! handshake enforcement, telemetry and the serve loop over a real socket.

use std::fs;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::*;
use crate::config::{Config, Project};
use crate::protocol::{Device, DeviceKind, SessionEvent};
use jaum_adapters::Executor;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "jaum-daemon-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    dir
}

fn app_in(dir: &Path) -> App {
    let backlog = dir.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for id in ["TASK-001", "TASK-002"] {
        fs::write(
            backlog.join(format!("{id}.md")),
            format!("---\nid: {id}\ntype: impl\nstatus: wip\n---\n\n## Objective\nx\n"),
        )
        .unwrap();
    }
    let project = Project {
        name: "t".into(),
        root: dir.to_path_buf(),
        backlog,
        docs: dir.join("docs"),
        work_dir: dir.join(".jaum"),
        repos: Vec::new(),
    };
    let mut app = App::new(
        Config {
            projects: vec![project],
            ci_poll_secs: None,
        },
        0,
    )
    .unwrap();
    // safety net: any executor spawn in a test becomes `cat`, never `claude`.
    app.executor = jaum_adapters::ClaudeExecutor::with_bin("cat");
    app
}

fn app() -> App {
    app_in(&tmp("app"))
}

fn device() -> Device {
    Device {
        name: "test-client".into(),
        kind: DeviceKind::Terminal,
    }
}

fn hello_msg() -> ClientMsg {
    ClientMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        device: device(),
        token: None,
    }
}

// --- Daemon (no socket) -----------------------------------------------------

#[test]
fn burst_of_intents_yields_a_single_snapshot() {
    let mut d = Daemon::new(app());
    // first call seeds the dedupe state
    assert!(d.snapshot_if_changed().is_some());
    assert!(d.snapshot_if_changed().is_none(), "no change, no snapshot");

    // a burst of state changes...
    d.apply(Intent::SelectNext);
    d.apply(Intent::OpenDetail);
    d.apply(Intent::DetailScrollDown);

    // ...coalesces into exactly one snapshot
    let snap = d.snapshot_if_changed().expect("one snapshot for the burst");
    assert_eq!(snap.board.selected, 1);
    assert!(snap.board.detail_open);
    assert_eq!(snap.board.detail_scroll, 1);
    assert!(d.snapshot_if_changed().is_none());
    assert!(d.last_snapshot().is_some());
}

#[test]
fn quit_becomes_detach_without_shutting_down() {
    let mut d = Daemon::new(app());
    let fx = d.apply(Intent::Quit);
    assert!(fx.detach, "quit should signal detach");
    assert!(!d.app_mut().should_quit, "should_quit should be reset");
    let fx = d.apply(Intent::SelectNext);
    assert!(!fx.detach);
}

#[test]
fn editor_request_roundtrip_and_tick() {
    let mut d = Daemon::new(app());
    assert_eq!(d.take_editor_request(), None);

    let fx = d.apply(Intent::EditConventions);
    let path = fx.editor.expect("editor requested");
    assert!(path.ends_with("conventions.md"));
    assert_eq!(d.take_editor_request(), None, "request must be cleared");

    d.editor_done();
    assert_eq!(d.app_mut().status_msg, "conventions.md updated");

    // tick only needs to not disturb the state
    d.tick();
}

#[test]
fn spawn_daemon_process_creates_log_and_detaches() {
    let dir = tmp("spawn");
    let log = dir.join("nested").join("daemon.log");
    spawn_daemon_process(Path::new("/usr/bin/true"), 3, &log).unwrap();
    assert!(log.exists(), "log file should be created");
    let _ = fs::remove_dir_all(&dir);

    // an empty log path has no parent and cannot be opened
    assert!(spawn_daemon_process(Path::new("/usr/bin/true"), 3, Path::new("")).is_err());
}

#[test]
fn shutdown_without_daemon_reports_false() {
    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "jaum-noserve-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(!is_running(&sock));
    assert!(!shutdown(&sock).unwrap());
}

#[test]
fn snapshot_rate_samples_stay_bounded_without_pings() {
    let mut server = Server::new(Daemon::new(app()));
    // stale samples piled up while nobody pinged for a status read
    for _ in 0..64 {
        server
            .snap_times
            .push_back(Instant::now() - Duration::from_secs(30));
    }
    // recording new broadcasts must drop the stale ones on its own
    for _ in 0..8 {
        server.note_snapshot();
    }
    assert!(
        server.snap_times.len() <= 8,
        "stale samples must be pruned on push, got {}",
        server.snap_times.len()
    );
    let status = server.status();
    assert!(status.snapshots_per_sec >= 0.0);
}

#[test]
fn burst_drain_yields_within_the_cap_under_continuous_events() {
    let mut server = Server::new(Daemon::new(app()));
    let (tx, rx) = channel::<Event>();
    // producer keeps the channel busy with sub-COALESCE gaps for ~2s
    let producer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if tx.send(Event::Disconnect(9999)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let started = std::time::Instant::now();
    drain_burst(&rx, &mut server);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(600),
        "burst must yield to tick/broadcast within the cap, took {elapsed:?}"
    );
    drop(rx);
    producer.join().unwrap();
}

// --- serve (socket) -----------------------------------------------------------

fn sock_path(tag: &str) -> PathBuf {
    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "jaum-{tag}-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    sock
}

fn connect(sock: &Path) -> (UnixStream, BufReader<UnixStream>) {
    let conn = loop {
        if let Ok(c) = UnixStream::connect(sock) {
            break c;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let reader = BufReader::new(conn.try_clone().unwrap());
    (conn, reader)
}

fn next_msg(reader: &mut BufReader<UnixStream>) -> ServerMsg {
    read_msg::<_, ServerMsg>(reader).unwrap().expect("message")
}

#[test]
fn serve_refuses_second_daemon_and_stops_on_shutdown() {
    let sock = sock_path("dup");
    // orphan socket file from a dead daemon: serve must clean it up
    fs::write(&sock, "stale").unwrap();

    let sock_c = sock.clone();
    let server = std::thread::spawn(move || serve(&sock_c, app()));
    for _ in 0..200 {
        if is_running(&sock) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(is_running(&sock));

    let err = serve(&sock, app()).unwrap_err();
    assert!(err.to_string().contains("already running"));

    assert!(shutdown(&sock).unwrap());
    server.join().unwrap().unwrap();
    assert!(!sock.exists());
}

#[test]
fn serve_rejects_incompatible_version_and_missing_hello() {
    let sock = sock_path("reject");
    let sock_c = sock.clone();
    let server = std::thread::spawn(move || serve(&sock_c, app()));

    // wrong protocol version
    let (mut conn, mut reader) = connect(&sock);
    write_msg(
        &mut conn,
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            device: device(),
            token: None,
        },
    )
    .unwrap();
    match next_msg(&mut reader) {
        ServerMsg::Rejected { reason } => {
            assert!(reason.contains("incompatible protocol"), "{reason}")
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    // the daemon drops the connection after rejecting
    assert!(read_msg::<_, ServerMsg>(&mut reader).unwrap().is_none());

    // traffic before Hello
    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &ClientMsg::Intent(Intent::SelectNext)).unwrap();
    match next_msg(&mut reader) {
        ServerMsg::Rejected { reason } => assert!(reason.contains("handshake"), "{reason}"),
        other => panic!("expected Rejected, got {other:?}"),
    }

    // a second Hello on an already-handshaken connection is a protocol error
    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &hello_msg()).unwrap();
    assert!(matches!(next_msg(&mut reader), ServerMsg::Welcome { .. }));
    assert!(matches!(next_msg(&mut reader), ServerMsg::Snapshot(_)));
    write_msg(&mut conn, &hello_msg()).unwrap();
    match next_msg(&mut reader) {
        ServerMsg::Rejected { reason } => {
            assert!(reason.contains("already completed"), "{reason}")
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(read_msg::<_, ServerMsg>(&mut reader).unwrap().is_none());

    // rejected clients never block a well-behaved one
    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &hello_msg()).unwrap();
    assert!(matches!(next_msg(&mut reader), ServerMsg::Welcome { .. }));

    write_msg(&mut conn, &ClientMsg::Shutdown).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn serve_handshake_coalescing_telemetry_editor_and_detach() {
    let sock = sock_path("flow");
    let sock_c = sock.clone();
    let server = std::thread::spawn(move || serve(&sock_c, app()));

    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &hello_msg()).unwrap();

    // handshake: Welcome with the shared protocol version, then ONE snapshot
    match next_msg(&mut reader) {
        ServerMsg::Welcome {
            protocol_version,
            device_id,
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(device_id, 0);
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
    let first = match next_msg(&mut reader) {
        ServerMsg::Snapshot(s) => s,
        other => panic!("expected Snapshot, got {other:?}"),
    };
    assert_eq!(first.board.tasks.len(), 2);
    assert_eq!(first.board.selected, 0);

    // a burst of intents becomes a SINGLE coalesced snapshot
    for intent in [
        Intent::SelectNext,
        Intent::OpenDetail,
        Intent::DetailScrollDown,
        Intent::DetailScrollDown,
    ] {
        write_msg(&mut conn, &ClientMsg::Intent(intent)).unwrap();
    }
    let coalesced = match next_msg(&mut reader) {
        ServerMsg::Snapshot(s) => s,
        other => panic!("expected Snapshot, got {other:?}"),
    };
    assert_eq!(coalesced.board.selected, 1);
    assert!(coalesced.board.detail_open);
    assert_eq!(coalesced.board.detail_scroll, 2);

    // ping -> pong with telemetry; the reported RTT lands in devices[]
    write_msg(
        &mut conn,
        &ClientMsg::Ping {
            seq: 41,
            last_rtt_ms: Some(3),
        },
    )
    .unwrap();
    // no state changed since the burst: the next message MUST be the pong
    // (a second snapshot here would mean the burst was not coalesced).
    match next_msg(&mut reader) {
        ServerMsg::Pong { seq, status } => {
            assert_eq!(seq, 41);
            assert_eq!(status.devices.len(), 1);
            assert_eq!(status.devices[0].name, "test-client");
            assert_eq!(status.devices[0].rtt_ms, Some(3));
            assert!(status.snapshots_per_sec >= 0.0);
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    // editor flow: intent -> RunEditor to this client -> EditorDone reloads
    write_msg(&mut conn, &ClientMsg::Intent(Intent::EditConventions)).unwrap();
    let mut got_editor = false;
    for _ in 0..10 {
        match next_msg(&mut reader) {
            ServerMsg::RunEditor { path } => {
                assert!(path.ends_with("conventions.md"));
                got_editor = true;
                break;
            }
            ServerMsg::Snapshot(_) => continue,
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(got_editor, "RunEditor never arrived");
    write_msg(&mut conn, &ClientMsg::EditorDone).unwrap();

    // quit: daemon answers Detach and survives for the next client
    write_msg(&mut conn, &ClientMsg::Intent(Intent::Quit)).unwrap();
    let mut got_detach = false;
    for _ in 0..10 {
        match next_msg(&mut reader) {
            ServerMsg::Detach => {
                got_detach = true;
                break;
            }
            ServerMsg::Snapshot(_) => continue,
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(got_detach, "Detach never arrived");
    drop(conn);

    // reattach: the state (selection, overlay) survived the detach
    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &hello_msg()).unwrap();
    assert!(matches!(next_msg(&mut reader), ServerMsg::Welcome { .. }));
    match next_msg(&mut reader) {
        ServerMsg::Snapshot(s) => assert_eq!(s.board.selected, 1),
        other => panic!("expected Snapshot, got {other:?}"),
    }

    write_msg(&mut conn, &ClientMsg::Shutdown).unwrap();
    server.join().unwrap().unwrap();
    assert!(!sock.exists(), "socket should be removed on shutdown");
}

#[test]
fn serve_broadcasts_session_events_after_handshake() {
    let dir = tmp("events");
    let mut app = app_in(&dir);
    // a live `cat` session opened BEFORE serve; events emitted while nobody is
    // attached are dropped (a fresh client gets the snapshot, not history).
    let session = jaum_adapters::ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &jaum_adapters::ExecFlags::default())
        .unwrap();
    app.open_session(
        crate::app::SessionKind::Play,
        Some("TASK-001".into()),
        session,
        Vec::new(),
        "uuid-1".into(),
        dir.clone(),
    );

    let sock = sock_path("events");
    let sock_c = sock.clone();
    let server = std::thread::spawn(move || serve(&sock_c, app));

    let (mut conn, mut reader) = connect(&sock);
    write_msg(&mut conn, &hello_msg()).unwrap();

    // finishing the session (an intent, post-handshake) must broadcast the
    // event to this client.
    write_msg(&mut conn, &ClientMsg::Intent(Intent::FinishSession)).unwrap();
    let mut finished = false;
    for _ in 0..200 {
        if let ServerMsg::Session(SessionEvent { session_id, kind }) = next_msg(&mut reader) {
            assert_eq!(session_id, "uuid-1");
            if matches!(kind, crate::protocol::SessionEventKind::Finished) {
                finished = true;
                break;
            }
        }
    }
    assert!(finished, "Finished event never arrived");

    write_msg(&mut conn, &ClientMsg::Shutdown).unwrap();
    server.join().unwrap().unwrap();
}
