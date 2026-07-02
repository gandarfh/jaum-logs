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

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::style::{Color, Modifier};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use protocol::{ClientMsg, ServerMsg, WireCell};

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
        expected,
        golden,
        "serialization of `{name}` diverged from the committed fixture"
    );

    let decoded: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("fixture {name}.json no longer decodes: {e}"));
    let reencoded = serde_json::to_value(&decoded).unwrap();
    assert_eq!(reencoded, golden, "roundtrip of `{name}` is lossy");
}

fn sample_cells() -> Vec<WireCell> {
    vec![
        WireCell {
            x: 0,
            y: 0,
            sym: "j".into(),
            fg: Color::Rgb(180, 142, 255),
            bg: Color::Reset,
            underline: Color::Reset,
            mods: Modifier::BOLD,
        },
        WireCell {
            x: 1,
            y: 0,
            sym: "◆".into(),
            fg: Color::Indexed(42),
            bg: Color::Black,
            underline: Color::Cyan,
            mods: Modifier::ITALIC | Modifier::UNDERLINED,
        },
    ]
}

#[test]
fn client_key() {
    let mut key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
    key.kind = KeyEventKind::Press;
    key.state = KeyEventState::NONE;
    check_golden("client_key", &ClientMsg::Key(key));
}

#[test]
fn client_mouse() {
    check_golden(
        "client_mouse",
        &ClientMsg::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }),
    );
}

#[test]
fn client_resize() {
    check_golden(
        "client_resize",
        &ClientMsg::Resize {
            cols: 120,
            rows: 40,
        },
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

#[test]
fn server_frame_full() {
    check_golden(
        "server_frame_full",
        &ServerMsg::FrameFull {
            cols: 80,
            rows: 24,
            cells: sample_cells(),
        },
    );
}

#[test]
fn server_frame_diff() {
    check_golden("server_frame_diff", &ServerMsg::FrameDiff(sample_cells()));
}

#[test]
fn server_detach() {
    check_golden("server_detach", &ServerMsg::Detach);
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

/// The attach handshake: the client announces its size, the daemon answers
/// with a full frame. The fixture pins the ordered pair as one document.
#[test]
fn handshake() {
    #[derive(Serialize, serde::Deserialize)]
    struct Handshake {
        client: ClientMsg,
        server: ServerMsg,
    }
    check_golden(
        "handshake",
        &Handshake {
            client: ClientMsg::Resize { cols: 80, rows: 24 },
            server: ServerMsg::FrameFull {
                cols: 80,
                rows: 24,
                cells: sample_cells(),
            },
        },
    );
}

/// The wire framing (4-byte big-endian length + JSON payload) around a fixture
/// payload, so non-Rust clients can validate their framing against real bytes.
#[test]
fn framing_wraps_fixture_payload() {
    let msg = ClientMsg::Resize {
        cols: 120,
        rows: 40,
    };
    let mut framed = Vec::new();
    protocol::write_msg(&mut framed, &msg).unwrap();

    let payload = serde_json::to_vec(&msg).unwrap();
    let mut expected = (payload.len() as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(framed, expected);

    let mut cur = std::io::Cursor::new(framed);
    let back: ClientMsg = protocol::read_msg(&mut cur).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&msg).unwrap()
    );
}
