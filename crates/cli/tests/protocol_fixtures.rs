//! Golden fixtures for the daemon-client wire protocol.
//!
//! Every message variant is serialized and compared against the JSON files
//! committed under `tests/fixtures/protocol/`. These fixtures are the contract
//! shared with non-Rust clients (Swift Codable decodes the same files), so a
//! failing test here means a wire-format break, not a test to update blindly.
//!
//! To regenerate after an intentional protocol change:
//! `UPDATE_PROTOCOL_FIXTURES=1 cargo test -p jaum-cli --test protocol_fixtures`

use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use protocol::{
    BoardView, CardView, ClientMsg, ConstraintView, DaemonStatus, Device, DeviceKind, DeviceStatus,
    DocsView, DomainSnapshot, EnforceId, FocusId, InputKind, InputView, Intent, JobView,
    OverlapView, PROTOCOL_VERSION, PickerView, PrView, ProjectRef, ServerMsg, SessionEvent,
    SessionEventKind, SessionKind, StatusId, TabId, TaskTypeId, TaskView,
};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol")
}

/// Compares `msg` against the committed fixture at the JSON value level
/// (whitespace-insensitive), then decodes the fixture back into `T` and
/// re-serializes to prove the roundtrip is lossless.
fn check_golden<T: Serialize + DeserializeOwned>(name: &str, msg: &T) {
    let path = fixture_dir().join(format!("{name}.json"));
    let expected = serde_json::to_value(msg).unwrap();

    if std::env::var_os("UPDATE_PROTOCOL_FIXTURES").is_some() {
        fs::create_dir_all(fixture_dir()).unwrap();
        let pretty = serde_json::to_string_pretty(&expected).unwrap();
        fs::write(&path, format!("{pretty}\n")).unwrap();
    }

    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
    let golden: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        expected, golden,
        "serialization of `{name}` diverged from the committed fixture"
    );

    let decoded: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("fixture {name}.json no longer decodes: {e}"));
    let reencoded = serde_json::to_value(&decoded).unwrap();
    assert_eq!(reencoded, golden, "roundtrip of `{name}` is lossy");
}

fn sample_device() -> Device {
    Device {
        name: "mbp-de-joao".into(),
        kind: DeviceKind::Terminal,
    }
}

fn sample_status() -> DaemonStatus {
    DaemonStatus {
        uptime_ms: 754_000,
        snapshots_per_sec: 2.5,
        devices: vec![
            DeviceStatus {
                id: 0,
                name: "mbp-de-joao".into(),
                kind: DeviceKind::Terminal,
                rtt_ms: Some(2),
            },
            DeviceStatus {
                id: 3,
                name: "iphone".into(),
                kind: DeviceKind::Iphone,
                rtt_ms: None,
            },
        ],
    }
}

/// A snapshot exercising every view type of the domain.
fn sample_snapshot() -> DomainSnapshot {
    DomainSnapshot {
        project: "proj".into(),
        projects: vec![ProjectRef {
            name: "proj".into(),
            backlog: "/home/u/jaum/proj/backlog".into(),
        }],
        tab: TabId::Board,
        board: BoardView {
            tasks: vec![TaskView {
                id: "TASK-001".into(),
                task_type: TaskTypeId::Impl,
                status: StatusId::Wip,
                rfcs: vec!["RFC-0001".into()],
                adrs: vec!["ADR-0002".into()],
                prs: vec![PrView {
                    repo: "org/x".into(),
                    pr: 42,
                    branch: "feat/thing".into(),
                }],
                deferred: vec!["extra scope".into()],
                constraints: vec![ConstraintView {
                    text: "do not touch src/legacy/".into(),
                    enforce: EnforceId::Hook,
                }],
                body: "## Objective\nDo the thing.\n".into(),
                live_session: true,
            }],
            selected: 0,
            project_selected: false,
            focus: FocusId::Cards,
            cards: vec![CardView::Session {
                kind: SessionKind::Play,
                live: true,
                last_activity_ms: 1_700_000_000_000,
            }],
            card_selected: 0,
            chat_fullscreen: false,
            setup_needed: true,
            setup_live: false,
            detail_open: false,
            detail_scroll: 0,
            overlaps: vec![OverlapView {
                a: "TASK-001".into(),
                b: "TASK-002".into(),
                repo: "org/x".into(),
            }],
        },
        docs: DocsView {
            dir: "/home/u/jaum/proj/docs".into(),
            list: vec!["rfcs/RFC-0001.md".into()],
            selected: 0,
            preview: "# RFC-0001\n".into(),
            doc_open: false,
            scroll: 0,
        },
        picker: Some(PickerView { selected: 0 }),
        input: Some(InputView {
            kind: InputKind::Convention,
            buffer: "tests first".into(),
        }),
        job: Some(JobView {
            title: "init".into(),
            logs: vec!["scanning docs".into()],
            finished: false,
            follow: true,
            scroll: 0,
        }),
        job_overlay: true,
        toast: Some("play started on TASK-001".into()),
    }
}

// --- client -> daemon ----------------------------------------------------------

#[test]
fn client_hello() {
    check_golden(
        "client_hello",
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            device: sample_device(),
            token: None,
        },
    );
}

#[test]
fn client_ping() {
    check_golden(
        "client_ping",
        &ClientMsg::Ping {
            seq: 7,
            last_rtt_ms: Some(3),
        },
    );
}

#[test]
fn client_intent_navigation() {
    check_golden(
        "client_intent_navigation",
        &ClientMsg::Intent(Intent::SelectNext),
    );
}

#[test]
fn client_intent_set_tab() {
    check_golden(
        "client_intent_set_tab",
        &ClientMsg::Intent(Intent::SetTab { index: 1 }),
    );
}

#[test]
fn client_intent_start_input() {
    check_golden(
        "client_intent_start_input",
        &ClientMsg::Intent(Intent::StartInput {
            kind: InputKind::InitPath,
            prefill: "/home/u/proj".into(),
        }),
    );
}

#[test]
fn client_intent_input_char() {
    check_golden(
        "client_intent_input_char",
        &ClientMsg::Intent(Intent::InputChar { ch: 'x' }),
    );
}

#[test]
fn client_editor_done() {
    check_golden("client_editor_done", &ClientMsg::EditorDone);
}

#[test]
fn client_shutdown() {
    check_golden("client_shutdown", &ClientMsg::Shutdown);
}

// --- daemon -> client ----------------------------------------------------------

#[test]
fn server_welcome() {
    check_golden(
        "server_welcome",
        &ServerMsg::Welcome {
            protocol_version: PROTOCOL_VERSION,
            device_id: 0,
        },
    );
}

#[test]
fn server_rejected() {
    check_golden(
        "server_rejected",
        &ServerMsg::Rejected {
            reason: "incompatible protocol: daemon speaks v1, client sent v2".into(),
        },
    );
}

#[test]
fn server_snapshot() {
    check_golden(
        "server_snapshot",
        &ServerMsg::Snapshot(Box::new(sample_snapshot())),
    );
}

#[test]
fn server_pong() {
    check_golden(
        "server_pong",
        &ServerMsg::Pong {
            seq: 7,
            status: sample_status(),
        },
    );
}

#[test]
fn server_session_started() {
    check_golden(
        "server_session_started",
        &ServerMsg::Session(SessionEvent {
            session_id: "3e0b9a6a-0000-4000-8000-000000000000".into(),
            kind: SessionEventKind::Started {
                kind: SessionKind::Play,
                task: Some("TASK-001".into()),
            },
        }),
    );
}

#[test]
fn server_session_output() {
    check_golden(
        "server_session_output",
        &ServerMsg::Session(SessionEvent {
            session_id: "3e0b9a6a-0000-4000-8000-000000000000".into(),
            kind: SessionEventKind::Output {
                bytes: vec![0x1b, 0x5b, 0x41, 0x6f, 0x6b],
            },
        }),
    );
}

#[test]
fn server_session_finished() {
    check_golden(
        "server_session_finished",
        &ServerMsg::Session(SessionEvent {
            session_id: "3e0b9a6a-0000-4000-8000-000000000000".into(),
            kind: SessionEventKind::Finished,
        }),
    );
}

#[test]
fn server_run_editor() {
    check_golden(
        "server_run_editor",
        &ServerMsg::RunEditor {
            path: "/tmp/conventions.md".into(),
        },
    );
}

#[test]
fn server_detach() {
    check_golden("server_detach", &ServerMsg::Detach);
}

/// The attach handshake: the client presents its device and protocol version,
/// the daemon accepts and the first snapshot follows. The fixture pins the
/// ordered exchange as one document.
#[test]
fn handshake() {
    #[derive(Serialize, serde::Deserialize)]
    struct Handshake {
        client: ClientMsg,
        server: ServerMsg,
        first_snapshot: ServerMsg,
    }
    check_golden(
        "handshake",
        &Handshake {
            client: ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                device: sample_device(),
                token: None,
            },
            server: ServerMsg::Welcome {
                protocol_version: PROTOCOL_VERSION,
                device_id: 0,
            },
            first_snapshot: ServerMsg::Snapshot(Box::new(sample_snapshot())),
        },
    );
}

/// The wire framing (4-byte big-endian length + JSON payload) around a fixture
/// payload, so non-Rust clients can validate their framing against real bytes.
#[test]
fn framing_wraps_fixture_payload() {
    let msg = ClientMsg::Ping {
        seq: 7,
        last_rtt_ms: Some(3),
    };
    let mut framed = Vec::new();
    protocol::write_msg(&mut framed, &msg).unwrap();

    let payload = serde_json::to_vec(&msg).unwrap();
    let mut expected = (payload.len() as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(framed, expected);

    let mut cur = std::io::Cursor::new(framed);
    let back: ClientMsg = protocol::read_msg(&mut cur).unwrap().unwrap();
    assert_eq!(back, msg);
}
