use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::json;

use super::*;

/// Fake sidecar speaking the JSONL protocol, scripted by the chat text.
/// Written once per process to avoid rewriting a file another test is using.
pub(crate) fn stub() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("jaum-sidecar-stub-{}.js", std::process::id()));
        fs::write(&path, STUB_JS).unwrap();
        path
    })
}

const STUB_JS: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (o) => process.stdout.write(JSON.stringify(o) + "\n");
const pending = new Map();
rl.on('line', (line) => {
  if (!line.trim()) return;
  let cmd;
  try { cmd = JSON.parse(line); } catch { return; }
  if (cmd.type === 'ping') { send({ type: 'pong' }); return; }
  if (cmd.type === 'abort') {
    send({ type: 'done', request_id: cmd.request_id, usage: null, stop_reason: 'aborted' });
    return;
  }
  if (cmd.type === 'permission_response') {
    const rid = pending.get(cmd.permission_id);
    if (rid) {
      pending.delete(cmd.permission_id);
      const suffix = cmd.decision.message ? ':' + cmd.decision.message : '';
      send({ type: 'done', request_id: rid, usage: null, stop_reason: cmd.decision.behavior + suffix });
    }
    return;
  }
  if (cmd.type !== 'chat') return;
  const rid = cmd.request_id;
  const text = cmd.content.map((b) => (b.type === 'text' ? b.text : '')).join(' ');
  if (text.includes('garbage-first')) process.stdout.write('this is not json\n');
  send({ type: 'session', request_id: rid, claude_session_id: cmd.resume ?? cmd.session_id });
  if (text.includes('env-check')) {
    const vars = ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'SIDECAR_HMAC_SECRET'];
    const message = vars.map((v) => process.env[v] ?? 'unset').join('|');
    send({ type: 'error', request_id: rid, category: 'internal', message });
    send({ type: 'done', request_id: rid, usage: null, stop_reason: 'end_turn' });
    return;
  }
  if (text.includes('ask-permission')) {
    const pid = 'perm-' + rid;
    pending.set(pid, rid);
    send({ type: 'permission_request', request_id: rid, permission_id: pid, tool_name: 'Write', tool_input: { file_path: '/tmp/x' } });
    return;
  }
  if (text.includes('hang')) {
    send({ type: 'text_delta', request_id: rid, text: 'hanging' });
    return;
  }
  send({ type: 'text_delta', request_id: rid, text: 'echo:' + text });
  send({ type: 'tool_use', request_id: rid, tool_use_id: 'tu1', name: 'Bash', input: { command: 'ls' } });
  send({ type: 'tool_result', request_id: rid, tool_use_id: 'tu1', content: [{ type: 'text', text: 'ok' }], is_error: false });
  send({ type: 'done', request_id: rid, usage: { input_tokens: 1, output_tokens: 2, cache_read_tokens: 3 }, stop_reason: 'end_turn' });
});
"#;

fn turn(request_id: &str, text: &str) -> ChatTurn {
    ChatTurn {
        request_id: request_id.into(),
        session_id: "11111111-1111-4111-8111-111111111111".into(),
        resume: None,
        cwd: None,
        model: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        system_prompt_append: None,
        guard_patterns: Vec::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn spawn_stub() -> SidecarClient {
    SidecarClient::spawn("node", stub()).expect("node stub spawns")
}

const RECV: Duration = Duration::from_secs(10);

fn collect_until_done(rx: &std::sync::mpsc::Receiver<SidecarEvent>) -> Vec<SidecarEvent> {
    let mut out = Vec::new();
    loop {
        let event = rx.recv_timeout(RECV).expect("event before timeout");
        let done = matches!(event, SidecarEvent::Done { .. });
        out.push(event);
        if done {
            return out;
        }
    }
}

// --- wire shapes ------------------------------------------------------------

#[test]
fn chat_command_serializes_with_explicit_nulls() {
    let cmd = SidecarCommand::Chat(turn("req-1", "hello"));
    let value = serde_json::to_value(&cmd).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "chat",
            "request_id": "req-1",
            "session_id": "11111111-1111-4111-8111-111111111111",
            "resume": null,
            "cwd": null,
            "model": null,
            "allowed_tools": [],
            "disallowed_tools": [],
            "system_prompt_append": null,
            "guard_patterns": [],
            "content": [{"type": "text", "text": "hello"}],
        })
    );
    let back: SidecarCommand = serde_json::from_value(value).unwrap();
    assert_eq!(back, cmd);
}

#[test]
fn control_commands_serialize_to_the_wire_shapes() {
    assert_eq!(
        serde_json::to_value(SidecarCommand::Ping).unwrap(),
        json!({"type": "ping"})
    );
    assert_eq!(
        serde_json::to_value(SidecarCommand::Abort {
            request_id: "r".into()
        })
        .unwrap(),
        json!({"type": "abort", "request_id": "r"})
    );
    assert_eq!(
        serde_json::to_value(SidecarCommand::PermissionResponse {
            permission_id: "p".into(),
            decision: PermissionDecision::Allow,
        })
        .unwrap(),
        json!({
            "type": "permission_response",
            "permission_id": "p",
            "decision": {"behavior": "allow"},
        })
    );
    assert_eq!(
        serde_json::to_value(PermissionDecision::Deny { message: None }).unwrap(),
        json!({"behavior": "deny"})
    );
    assert_eq!(
        serde_json::to_value(PermissionDecision::Deny {
            message: Some("no".into())
        })
        .unwrap(),
        json!({"behavior": "deny", "message": "no"})
    );
}

#[test]
fn events_deserialize_from_wire_json() {
    let cases: Vec<(&str, SidecarEvent)> = vec![
        (
            r#"{"type":"session","request_id":"r","claude_session_id":"c"}"#,
            SidecarEvent::Session {
                request_id: "r".into(),
                claude_session_id: "c".into(),
            },
        ),
        (
            r#"{"type":"text_delta","request_id":"r","text":"hi"}"#,
            SidecarEvent::TextDelta {
                request_id: "r".into(),
                text: "hi".into(),
            },
        ),
        (
            r#"{"type":"done","request_id":"r","usage":null,"stop_reason":"end_turn"}"#,
            SidecarEvent::Done {
                request_id: "r".into(),
                usage: None,
                stop_reason: Some("end_turn".into()),
            },
        ),
        (r#"{"type":"pong"}"#, SidecarEvent::Pong),
    ];
    for (raw, expected) in cases {
        let event: SidecarEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event, expected);
    }
}

#[test]
fn request_id_covers_every_variant() {
    let with_id = SidecarEvent::Error {
        request_id: "r".into(),
        category: "auth".into(),
        message: "m".into(),
    };
    assert_eq!(with_id.request_id(), Some("r"));
    assert_eq!(SidecarEvent::Pong.request_id(), None);
}

#[test]
fn to_session_event_maps_chat_events_and_skips_bookkeeping() {
    let usage = Usage {
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 3,
    };
    let cases: Vec<(SidecarEvent, Option<SessionEvent>)> = vec![
        (
            SidecarEvent::TextDelta {
                request_id: "r".into(),
                text: "t".into(),
            },
            Some(SessionEvent::TextDelta { text: "t".into() }),
        ),
        (
            SidecarEvent::ToolUse {
                request_id: "r".into(),
                tool_use_id: "tu".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            },
            Some(SessionEvent::ToolUse {
                tool_use_id: "tu".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            }),
        ),
        (
            SidecarEvent::ToolResult {
                request_id: "r".into(),
                tool_use_id: "tu".into(),
                content: vec![ContentBlock::Text { text: "ok".into() }],
                is_error: true,
            },
            Some(SessionEvent::ToolResult {
                tool_use_id: "tu".into(),
                content: vec![ContentBlock::Text { text: "ok".into() }],
                is_error: true,
            }),
        ),
        (
            SidecarEvent::PermissionRequest {
                request_id: "r".into(),
                permission_id: "p".into(),
                tool_name: "Write".into(),
                tool_input: json!({}),
            },
            Some(SessionEvent::PermissionRequest {
                permission_id: "p".into(),
                tool_name: "Write".into(),
                tool_input: json!({}),
            }),
        ),
        (
            SidecarEvent::Done {
                request_id: "r".into(),
                usage: Some(usage),
                stop_reason: Some("end_turn".into()),
            },
            Some(SessionEvent::Done { usage: Some(usage) }),
        ),
        (
            SidecarEvent::Error {
                request_id: "r".into(),
                category: "auth".into(),
                message: "m".into(),
            },
            Some(SessionEvent::Error {
                category: "auth".into(),
                message: "m".into(),
            }),
        ),
        (
            SidecarEvent::Session {
                request_id: "r".into(),
                claude_session_id: "c".into(),
            },
            None,
        ),
        (SidecarEvent::Pong, None),
    ];
    for (event, expected) in cases {
        assert_eq!(event.to_session_event(), expected);
    }
}

// --- permission tracker -----------------------------------------------------

#[test]
fn tracker_counts_pending_requests() {
    let mut tracker = PermissionTracker::default();
    assert_eq!(tracker.pending_count(), 0);
    tracker.track("sess", "perm-1");
    assert_eq!(tracker.pending_count(), 1);
}

#[test]
fn tracker_expires_only_past_deadline() {
    let mut tracker = PermissionTracker::new(Duration::ZERO);
    tracker.track("sess-a", "perm-a");
    tracker.track("sess-b", "perm-b");
    let expired = tracker.expired(std::time::Instant::now());
    assert_eq!(expired.len(), 2);
    assert_eq!(expired[0].session_id, "sess-a");
    assert_eq!(expired[0].permission_id, "perm-a");
    assert_eq!(tracker.pending_count(), 0);

    let mut patient = PermissionTracker::new(Duration::from_secs(3600));
    patient.track("sess", "perm");
    assert!(patient.expired(std::time::Instant::now()).is_empty());
    assert_eq!(patient.pending_count(), 1);
}

#[test]
fn default_timeout_is_two_minutes() {
    assert_eq!(PERMISSION_TIMEOUT, Duration::from_secs(120));
}

// --- bundle resolution --------------------------------------------------------

#[test]
fn resolve_bundle_prefers_the_override() {
    let home = Path::new("/home/u/jaum");
    assert_eq!(
        resolve_bundle(Some("/custom/s.mjs"), home),
        PathBuf::from("/custom/s.mjs")
    );
    assert_eq!(
        resolve_bundle(None, home),
        PathBuf::from("/home/u/jaum/sidecar/jaum-sidecar.mjs")
    );
    assert_eq!(
        resolve_bundle(Some("  "), home),
        PathBuf::from("/home/u/jaum/sidecar/jaum-sidecar.mjs")
    );
}

// --- process integration (node stub) ----------------------------------------

#[test]
fn chat_receives_the_full_event_sequence() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-1", "hello")).unwrap();
    let events = collect_until_done(&rx);
    assert_eq!(
        events,
        vec![
            SidecarEvent::Session {
                request_id: "req-1".into(),
                claude_session_id: "11111111-1111-4111-8111-111111111111".into(),
            },
            SidecarEvent::TextDelta {
                request_id: "req-1".into(),
                text: "echo:hello".into(),
            },
            SidecarEvent::ToolUse {
                request_id: "req-1".into(),
                tool_use_id: "tu1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            },
            SidecarEvent::ToolResult {
                request_id: "req-1".into(),
                tool_use_id: "tu1".into(),
                content: vec![ContentBlock::Text { text: "ok".into() }],
                is_error: false,
            },
            SidecarEvent::Done {
                request_id: "req-1".into(),
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 3,
                }),
                stop_reason: Some("end_turn".into()),
            },
        ]
    );
    client.unregister("req-1");
}

#[test]
fn resume_id_is_reflected_as_the_claude_session() {
    let client = spawn_stub();
    let mut resumed = turn("req-r", "hello");
    resumed.resume = Some("prior-claude-session".into());
    let rx = client.chat(resumed).unwrap();
    let events = collect_until_done(&rx);
    assert_eq!(
        events.first(),
        Some(&SidecarEvent::Session {
            request_id: "req-r".into(),
            claude_session_id: "prior-claude-session".into(),
        })
    );
}

#[test]
fn concurrent_chats_are_demultiplexed_by_request_id() {
    let client = spawn_stub();
    let rx_a = client.chat(turn("req-a", "alpha")).unwrap();
    let rx_b = client.chat(turn("req-b", "beta")).unwrap();
    let events_b = collect_until_done(&rx_b);
    let events_a = collect_until_done(&rx_a);
    assert!(events_a.iter().all(|e| e.request_id() == Some("req-a")));
    assert!(events_b.iter().all(|e| e.request_id() == Some("req-b")));
    assert!(
        events_a
            .iter()
            .any(|e| matches!(e, SidecarEvent::TextDelta { text, .. } if text == "echo:alpha"))
    );
    assert!(
        events_b
            .iter()
            .any(|e| matches!(e, SidecarEvent::TextDelta { text, .. } if text == "echo:beta"))
    );
}

#[test]
fn permission_request_is_routed_and_resolved_by_the_response() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-p", "ask-permission")).unwrap();

    // session first, then the routed permission request.
    let _session = rx.recv_timeout(RECV).unwrap();
    let request = rx.recv_timeout(RECV).unwrap();
    let SidecarEvent::PermissionRequest {
        permission_id,
        tool_name,
        ..
    } = &request
    else {
        panic!("expected a permission request, got {request:?}");
    };
    assert_eq!(tool_name, "Write");

    client
        .respond_permission(permission_id, PermissionDecision::Allow)
        .unwrap();
    let done = rx.recv_timeout(RECV).unwrap();
    assert_eq!(
        done,
        SidecarEvent::Done {
            request_id: "req-p".into(),
            usage: None,
            stop_reason: Some("allow".into()),
        }
    );
}

#[test]
fn deny_with_message_reaches_the_sidecar() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-d", "ask-permission")).unwrap();
    let _session = rx.recv_timeout(RECV).unwrap();
    let request = rx.recv_timeout(RECV).unwrap();
    let SidecarEvent::PermissionRequest { permission_id, .. } = &request else {
        panic!("expected a permission request");
    };
    client
        .respond_permission(
            permission_id,
            PermissionDecision::Deny {
                message: Some("blocked".into()),
            },
        )
        .unwrap();
    let done = rx.recv_timeout(RECV).unwrap();
    assert_eq!(
        done,
        SidecarEvent::Done {
            request_id: "req-d".into(),
            usage: None,
            stop_reason: Some("deny:blocked".into()),
        }
    );
}

#[test]
fn abort_interrupts_a_hanging_query() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-h", "hang")).unwrap();
    let _session = rx.recv_timeout(RECV).unwrap();
    let _delta = rx.recv_timeout(RECV).unwrap();
    client.abort("req-h").unwrap();
    let done = rx.recv_timeout(RECV).unwrap();
    assert_eq!(
        done,
        SidecarEvent::Done {
            request_id: "req-h".into(),
            usage: None,
            stop_reason: Some("aborted".into()),
        }
    );
}

#[test]
fn ping_gets_a_pong() {
    let client = spawn_stub();
    assert!(!client.pong_seen());
    client.ping().unwrap();
    let deadline = std::time::Instant::now() + RECV;
    while !client.pong_seen() {
        assert!(
            std::time::Instant::now() < deadline,
            "no pong before timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn malformed_sidecar_output_is_skipped() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-g", "garbage-first")).unwrap();
    let events = collect_until_done(&rx);
    assert!(matches!(events.first(), Some(SidecarEvent::Session { .. })));
    assert!(matches!(events.last(), Some(SidecarEvent::Done { .. })));
}

#[test]
fn key_and_hmac_variables_never_reach_the_sidecar() {
    let client = spawn_stub();
    let rx = client.chat(turn("req-e", "env-check")).unwrap();
    let events = collect_until_done(&rx);
    assert!(
        events.contains(&SidecarEvent::Error {
            request_id: "req-e".into(),
            category: "internal".into(),
            message: "unset|unset|unset".into(),
        }),
        "scrubbed env vars leaked into the sidecar: {events:?}"
    );
}

#[test]
fn kill_makes_the_client_dead_and_sends_fail() {
    let mut client = spawn_stub();
    assert!(client.is_alive());
    client.kill();
    assert!(!client.is_alive());
    // The first write after the death may still land in the pipe buffer;
    // within a bounded window the broken pipe must surface.
    let deadline = std::time::Instant::now() + RECV;
    let mut i = 0;
    loop {
        i += 1;
        if client.chat(turn(&format!("req-x{i}"), "hello")).is_err() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sends never failed after kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
