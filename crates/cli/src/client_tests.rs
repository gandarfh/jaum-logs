//! Behavior tests for the client loop and its reader thread: handshake,
//! snapshot redraws, key → intent forwarding, editor round-trip and pings.

use std::collections::VecDeque;
use std::io::{self, Cursor, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::*;
use crate::protocol::{DaemonStatus, Intent, SessionEvent, SessionEventKind};

fn sample() -> DomainSnapshot {
    crate::protocol::tests::sample_snapshot()
}

/// Sample snapshot stripped down to plain board navigation (no overlays).
fn board_snapshot() -> DomainSnapshot {
    let mut s = sample();
    s.picker = None;
    s.input = None;
    s.job = None;
    s.job_overlay = false;
    s.board.detail_open = false;
    s.docs.doc_open = false;
    s.board.focus = crate::protocol::FocusId::Tasks;
    s
}

fn slot_with(s: Option<DomainSnapshot>) -> Arc<Mutex<Option<DomainSnapshot>>> {
    Arc::new(Mutex::new(s))
}

fn recv(srx: &Receiver<SrvEvent>) -> SrvEvent {
    srx.recv_timeout(Duration::from_secs(5)).expect("event")
}

fn sent_msgs(bytes: Vec<u8>) -> Vec<ClientMsg> {
    let mut cur = Cursor::new(bytes);
    let mut out = Vec::new();
    while let Some(m) = read_msg::<_, ClientMsg>(&mut cur).unwrap() {
        out.push(m);
    }
    out
}

/// A long interval so loop tests do not emit pings unless asked to.
const NO_PING: Duration = Duration::from_secs(3600);

// --- hello (handshake) --------------------------------------------------------

fn server_answer(msg: &ServerMsg) -> Cursor<Vec<u8>> {
    let mut buf = Vec::new();
    write_msg(&mut buf, msg).unwrap();
    Cursor::new(buf)
}

#[test]
fn hello_accepts_welcome_and_reports_rejection() {
    let device = || Device {
        name: "t".into(),
        kind: DeviceKind::Terminal,
    };

    let mut wire = Vec::new();
    let mut r = server_answer(&ServerMsg::Welcome {
        protocol_version: PROTOCOL_VERSION,
        device_id: 9,
    });
    assert_eq!(hello(&mut wire, &mut r, device()).unwrap(), 9);
    // the Hello actually went over the wire, version included
    match &sent_msgs(wire)[0] {
        ClientMsg::Hello {
            protocol_version,
            device,
            token,
        } => {
            assert_eq!(*protocol_version, PROTOCOL_VERSION);
            assert_eq!(device.name, "t");
            assert!(token.is_none());
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    let mut wire = Vec::new();
    let mut r = server_answer(&ServerMsg::Rejected {
        reason: "incompatible protocol".into(),
    });
    let err = hello(&mut wire, &mut r, device()).unwrap_err();
    assert!(err.to_string().contains("incompatible protocol"));

    // EOF during the handshake
    let mut wire = Vec::new();
    let mut r = Cursor::new(Vec::new());
    assert!(hello(&mut wire, &mut r, device()).is_err());
}

// --- spawn_reader --------------------------------------------------------------

#[test]
fn reader_stores_snapshots_and_forwards_events() {
    let (client_end, mut server_end) = UnixStream::pair().unwrap();
    let snap = slot_with(None);
    let (stx, srx) = channel();
    spawn_reader(BufReader::new(client_end), snap.clone(), stx);

    write_msg(&mut server_end, &ServerMsg::Snapshot(Box::new(sample()))).unwrap();
    assert!(matches!(recv(&srx), SrvEvent::Snapshot));
    assert_eq!(*snap.lock().unwrap(), Some(sample()));

    write_msg(
        &mut server_end,
        &ServerMsg::Pong {
            seq: 5,
            status: DaemonStatus {
                uptime_ms: 1,
                snapshots_per_sec: 0.0,
                devices: vec![],
            },
        },
    )
    .unwrap();
    assert!(matches!(recv(&srx), SrvEvent::Pong { seq: 5 }));

    // session events and late handshake frames are skipped without an event
    write_msg(
        &mut server_end,
        &ServerMsg::Session(SessionEvent {
            session_id: "u".into(),
            kind: SessionEventKind::Finished,
        }),
    )
    .unwrap();
    write_msg(
        &mut server_end,
        &ServerMsg::Welcome {
            protocol_version: PROTOCOL_VERSION,
            device_id: 1,
        },
    )
    .unwrap();
    write_msg(&mut server_end, &ServerMsg::Rejected { reason: "x".into() }).unwrap();
    write_msg(
        &mut server_end,
        &ServerMsg::RunEditor {
            path: "/tmp/conventions.md".into(),
        },
    )
    .unwrap();
    assert!(matches!(recv(&srx), SrvEvent::Editor(p) if p == "/tmp/conventions.md"));

    write_msg(&mut server_end, &ServerMsg::Detach).unwrap();
    assert!(matches!(recv(&srx), SrvEvent::Detach));
}

#[test]
fn reader_treats_eof_as_detach() {
    let (client_end, server_end) = UnixStream::pair().unwrap();
    let (stx, srx) = channel();
    spawn_reader(BufReader::new(client_end), slot_with(None), stx);
    drop(server_end);
    assert!(matches!(recv(&srx), SrvEvent::Detach));
}

#[test]
fn reader_treats_bad_frame_as_detach() {
    use std::io::Write as _;
    let (client_end, mut server_end) = UnixStream::pair().unwrap();
    let (stx, srx) = channel();
    spawn_reader(BufReader::new(client_end), slot_with(None), stx);
    server_end.write_all(&4u32.to_be_bytes()).unwrap();
    server_end.write_all(b"zzzz").unwrap();
    server_end.flush().unwrap();
    assert!(matches!(recv(&srx), SrvEvent::Detach));
}

// --- client_loop ------------------------------------------------------------

/// Scripted Ui: returns queued input events; when the script runs out it
/// sends `Detach` so the loop terminates.
struct ScriptUi {
    polls: VecDeque<Event>,
    stx: Sender<SrvEvent>,
    drawn: Vec<DomainSnapshot>,
    editors: Vec<String>,
    fail_editor: bool,
    fail_draw: bool,
    /// Injects a `Pong { seq }` into the loop on the first poll (simulates the
    /// daemon answering the first ping).
    pong_on_first_poll: Option<u64>,
}

impl ScriptUi {
    fn new(stx: Sender<SrvEvent>, polls: Vec<Event>) -> Self {
        Self {
            polls: polls.into(),
            stx,
            drawn: Vec::new(),
            editors: Vec::new(),
            fail_editor: false,
            fail_draw: false,
            pong_on_first_poll: None,
        }
    }
}

impl Ui for ScriptUi {
    fn draw(&mut self, snap: &DomainSnapshot) -> Result<()> {
        if self.fail_draw {
            anyhow::bail!("draw failed");
        }
        self.drawn.push(snap.clone());
        Ok(())
    }

    fn poll_event(&mut self, _timeout: Duration) -> Result<Option<Event>> {
        if let Some(seq) = self.pong_on_first_poll.take() {
            let _ = self.stx.send(SrvEvent::Pong { seq });
            return Ok(None);
        }
        match self.polls.pop_front() {
            Some(ev) => Ok(Some(ev)),
            None => {
                let _ = self.stx.send(SrvEvent::Detach);
                Ok(None)
            }
        }
    }

    fn run_editor(&mut self, path: &str) -> Result<()> {
        if self.fail_editor {
            anyhow::bail!("editor failed");
        }
        self.editors.push(path.to_string());
        Ok(())
    }
}

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn release(c: char) -> Event {
    let mut k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    k.kind = KeyEventKind::Release;
    Event::Key(k)
}

#[test]
fn loop_redraws_and_maps_keys_until_detach() {
    let (stx, srx) = channel();
    let snap = slot_with(Some(board_snapshot()));
    stx.send(SrvEvent::Snapshot).unwrap();

    let mut ui = ScriptUi::new(
        stx,
        vec![
            press('j'),
            release('j'), // ignored: not a Press
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }), // ignored: no session panel yet
            press('q'),
            Event::Resize(50, 20), // local redraw only
            Event::FocusGained,    // ignored: unhandled variant
        ],
    );
    let mut wire = Vec::new();
    client_loop(&mut ui, &mut wire, &srx, &snap, NO_PING).unwrap();

    assert!(!ui.drawn.is_empty());
    assert_eq!(ui.drawn[0], board_snapshot());

    let msgs = sent_msgs(wire);
    // the first liveness ping goes out right away, then only key presses
    // become intents
    assert!(matches!(msgs[0], ClientMsg::Ping { seq: 0, .. }));
    assert_eq!(msgs[1], ClientMsg::Intent(Intent::SelectNext));
    assert_eq!(msgs[2], ClientMsg::Intent(Intent::Quit));
    assert_eq!(msgs.len(), 3, "release/mouse/focus are dropped: {msgs:?}");
}

#[test]
fn loop_without_snapshot_sends_no_intents() {
    let (stx, srx) = channel();
    let snap = slot_with(None);
    let mut ui = ScriptUi::new(stx, vec![press('j')]);
    let mut wire = Vec::new();
    client_loop(&mut ui, &mut wire, &srx, &snap, NO_PING).unwrap();
    assert!(ui.drawn.is_empty(), "nothing to draw before a snapshot");
    let msgs = sent_msgs(wire);
    assert_eq!(msgs.len(), 1, "no context, no intents: {msgs:?}");
    assert!(matches!(msgs[0], ClientMsg::Ping { .. }));
}

#[test]
fn loop_runs_editor_then_reports_done() {
    let (stx, srx) = channel();
    let snap = slot_with(Some(board_snapshot()));
    stx.send(SrvEvent::Editor("/tmp/conv.md".into())).unwrap();

    let mut ui = ScriptUi::new(stx, Vec::new());
    let mut wire = Vec::new();
    client_loop(&mut ui, &mut wire, &srx, &snap, NO_PING).unwrap();

    assert_eq!(ui.editors, vec!["/tmp/conv.md".to_string()]);
    assert_eq!(ui.drawn.len(), 1, "editor return forces a redraw");
    let msgs = sent_msgs(wire);
    assert_eq!(msgs[0], ClientMsg::EditorDone);
}

#[test]
fn loop_pings_and_reports_measured_rtt() {
    let (stx, srx) = channel();
    let snap = slot_with(None);
    let mut ui = ScriptUi::new(stx, vec![Event::FocusGained, Event::FocusGained]);
    ui.pong_on_first_poll = Some(0);
    let mut wire = Vec::new();
    client_loop(&mut ui, &mut wire, &srx, &snap, Duration::ZERO).unwrap();

    let msgs = sent_msgs(wire);
    // first ping has no RTT yet; after the pong lands, the next one reports it
    assert!(matches!(
        msgs[0],
        ClientMsg::Ping {
            seq: 0,
            last_rtt_ms: None
        }
    ));
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ClientMsg::Ping {
                seq: 1..,
                last_rtt_ms: Some(_)
            }
        )),
        "no ping carried the measured RTT: {msgs:?}"
    );
}

#[test]
fn loop_propagates_editor_failure() {
    let (stx, srx) = channel();
    let snap = slot_with(None);
    stx.send(SrvEvent::Editor("/tmp/conv.md".into())).unwrap();
    let mut ui = ScriptUi::new(stx, Vec::new());
    ui.fail_editor = true;
    let mut wire = Vec::new();
    assert!(client_loop(&mut ui, &mut wire, &srx, &snap, NO_PING).is_err());
}

#[test]
fn loop_propagates_draw_failure() {
    let (stx, srx) = channel();
    let snap = slot_with(Some(board_snapshot()));
    stx.send(SrvEvent::Snapshot).unwrap();
    let mut ui = ScriptUi::new(stx, Vec::new());
    ui.fail_draw = true;
    let mut wire = Vec::new();
    assert!(client_loop(&mut ui, &mut wire, &srx, &snap, NO_PING).is_err());
}

/// Accepts writes but fails on flush, like a socket whose peer vanished.
struct FailWriter;

impl Write for FailWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("broken pipe"))
    }
}

#[test]
fn loop_propagates_socket_write_failure() {
    let (stx, srx) = channel();
    let snap = slot_with(None);
    // the immediate ping hits the broken socket right away
    let mut ui = ScriptUi::new(stx, Vec::new());
    assert!(client_loop(&mut ui, &mut FailWriter, &srx, &snap, Duration::ZERO).is_err());
}
