//! Golden fixtures for the daemon⇄sidecar wire protocol and the domain
//! `SessionEvent`. The JSON files under `tests/fixtures/sidecar/` are the
//! contract shared with the TypeScript sidecar (its tests decode the same
//! files), so a failing test here means a wire-format break.
//!
//! To regenerate after an intentional protocol change:
//! `UPDATE_PROTOCOL_FIXTURES=1 cargo test -p jaum-cli --test sidecar_fixtures`

use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

#[path = "../src/session_event.rs"]
#[allow(dead_code)]
mod session_event;
#[path = "../src/sidecar.rs"]
#[allow(dead_code)]
mod sidecar;

use session_event::{ContentBlock, ImageSource, SessionEvent, Usage};
use sidecar::{ChatTurn, GuardPattern, PermissionDecision, SidecarCommand, SidecarEvent};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sidecar")
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

fn image() -> ImageSource {
    ImageSource {
        kind: "base64".into(),
        media_type: "image/png".into(),
        data: "aWpn".into(),
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 3,
    }
}

// --- commands (daemon -> sidecar) -------------------------------------------

#[test]
fn command_chat() {
    check_golden(
        "command_chat",
        &SidecarCommand::Chat(ChatTurn {
            request_id: "11111111-1111-4111-8111-111111111111#1".into(),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            resume: None,
            cwd: Some("/home/u/repo.worktrees/feat-x".into()),
            model: Some("claude-fable-5".into()),
            allowed_tools: vec![],
            disallowed_tools: vec!["Bash(git merge)".into(), "Bash(gh pr merge)".into()],
            system_prompt_append: Some("Project conventions (always apply): ...".into()),
            guard_patterns: vec![GuardPattern {
                pattern: "src/legacy/".into(),
                reason: "do not touch src/legacy/".into(),
            }],
            content: vec![
                ContentBlock::Text {
                    text: "do the thing".into(),
                },
                ContentBlock::Image { source: image() },
            ],
        }),
    );
}

#[test]
fn command_chat_resume() {
    check_golden(
        "command_chat_resume",
        &SidecarCommand::Chat(ChatTurn {
            request_id: "11111111-1111-4111-8111-111111111111#2".into(),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            resume: Some("11111111-1111-4111-8111-111111111111".into()),
            cwd: None,
            model: None,
            allowed_tools: vec!["Read".into()],
            disallowed_tools: vec![],
            system_prompt_append: None,
            guard_patterns: vec![],
            content: vec![ContentBlock::Text {
                text: "follow up".into(),
            }],
        }),
    );
}

#[test]
fn command_permission_response() {
    check_golden(
        "command_permission_response_allow",
        &SidecarCommand::PermissionResponse {
            permission_id: "perm_1".into(),
            decision: PermissionDecision::Allow,
        },
    );
    check_golden(
        "command_permission_response_deny",
        &SidecarCommand::PermissionResponse {
            permission_id: "perm_2".into(),
            decision: PermissionDecision::Deny {
                message: Some("no decision within the deadline; denied by default".into()),
            },
        },
    );
}

#[test]
fn command_abort_and_ping() {
    check_golden(
        "command_abort",
        &SidecarCommand::Abort {
            request_id: "11111111-1111-4111-8111-111111111111#1".into(),
        },
    );
    check_golden("command_ping", &SidecarCommand::Ping);
}

// --- events (sidecar -> daemon) ----------------------------------------------

#[test]
fn event_session() {
    check_golden(
        "event_session",
        &SidecarEvent::Session {
            request_id: "r#1".into(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".into(),
        },
    );
}

#[test]
fn event_text_delta() {
    check_golden(
        "event_text_delta",
        &SidecarEvent::TextDelta {
            request_id: "r#1".into(),
            text: "hello ".into(),
        },
    );
}

#[test]
fn event_tool_use_and_result() {
    check_golden(
        "event_tool_use",
        &SidecarEvent::ToolUse {
            request_id: "r#1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "cargo test"}),
        },
    );
    check_golden(
        "event_tool_result",
        &SidecarEvent::ToolResult {
            request_id: "r#1".into(),
            tool_use_id: "toolu_1".into(),
            content: vec![
                ContentBlock::Text { text: "ok".into() },
                ContentBlock::Image { source: image() },
            ],
            is_error: false,
        },
    );
}

#[test]
fn event_permission_request() {
    check_golden(
        "event_permission_request",
        &SidecarEvent::PermissionRequest {
            request_id: "r#1".into(),
            permission_id: "perm_1".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "/tmp/x"}),
        },
    );
}

#[test]
fn event_done_and_error() {
    check_golden(
        "event_done",
        &SidecarEvent::Done {
            request_id: "r#1".into(),
            usage: Some(usage()),
            stop_reason: Some("end_turn".into()),
        },
    );
    check_golden(
        "event_error",
        &SidecarEvent::Error {
            request_id: "r#1".into(),
            category: "auth".into(),
            message: "log in with the claude CLI".into(),
        },
    );
    check_golden("event_pong", &SidecarEvent::Pong);
}

// --- domain session events (daemon -> clients + .sessions log) ---------------

#[test]
fn session_events() {
    check_golden(
        "session_event_text_delta",
        &SessionEvent::TextDelta { text: "hi".into() },
    );
    check_golden(
        "session_event_tool_use",
        &SessionEvent::ToolUse {
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        },
    );
    check_golden(
        "session_event_tool_result",
        &SessionEvent::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: vec![ContentBlock::Text { text: "ok".into() }],
            is_error: false,
        },
    );
    check_golden(
        "session_event_image",
        &SessionEvent::Image { source: image() },
    );
    check_golden(
        "session_event_permission_request",
        &SessionEvent::PermissionRequest {
            permission_id: "perm_1".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "/tmp/x"}),
        },
    );
    check_golden(
        "session_event_permission_decision_allow",
        &SessionEvent::PermissionDecision {
            permission_id: "perm_1".into(),
            behavior: "allow".into(),
            message: None,
        },
    );
    check_golden(
        "session_event_permission_decision_deny",
        &SessionEvent::PermissionDecision {
            permission_id: "perm_2".into(),
            behavior: "deny".into(),
            message: Some("no decision within the deadline; denied by default".into()),
        },
    );
    check_golden(
        "session_event_done",
        &SessionEvent::Done {
            usage: Some(usage()),
        },
    );
    check_golden(
        "session_event_error",
        &SessionEvent::Error {
            category: "permission_timeout".into(),
            message: "permission perm_1 denied by default after the deadline".into(),
        },
    );
}
