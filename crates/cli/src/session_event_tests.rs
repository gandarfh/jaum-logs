use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use super::*;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-selog-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn log_file(home: &std::path::Path, id: &str) -> PathBuf {
    home.join(".sessions").join(format!("{id}.jsonl"))
}

fn sample_events() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TextDelta { text: "hi".into() },
        SessionEvent::ToolUse {
            tool_use_id: "tu-1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        },
        SessionEvent::ToolResult {
            tool_use_id: "tu-1".into(),
            content: vec![
                ContentBlock::Text { text: "ok".into() },
                ContentBlock::Image {
                    source: ImageSource {
                        kind: "base64".into(),
                        media_type: "image/png".into(),
                        data: "aWpn".into(),
                    },
                },
            ],
            is_error: false,
        },
        SessionEvent::Image {
            source: ImageSource {
                kind: "base64".into(),
                media_type: "image/jpeg".into(),
                data: "ZGF0YQ==".into(),
            },
        },
        SessionEvent::PermissionRequest {
            permission_id: "perm_1".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "/tmp/x"}),
        },
        SessionEvent::PermissionDecision {
            permission_id: "perm_1".into(),
            behavior: "deny".into(),
            message: Some("denied by default".into()),
        },
        SessionEvent::Done {
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 3,
            }),
        },
        SessionEvent::Error {
            category: "auth".into(),
            message: "log in".into(),
        },
    ]
}

#[test]
fn events_serialize_with_snake_case_tags() {
    let tags: Vec<String> = sample_events()
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        tags,
        [
            "text_delta",
            "tool_use",
            "tool_result",
            "image",
            "permission_request",
            "permission_decision",
            "done",
            "error"
        ]
    );
    // done without usage keeps the field explicit as null.
    let done = serde_json::to_value(SessionEvent::Done { usage: None }).unwrap();
    assert_eq!(done, json!({"type": "done", "usage": null}));
    // an allow decision has no message and omits the field entirely.
    let allow = serde_json::to_value(SessionEvent::PermissionDecision {
        permission_id: "perm_1".into(),
        behavior: "allow".into(),
        message: None,
    })
    .unwrap();
    assert_eq!(
        allow,
        json!({"type": "permission_decision", "permission_id": "perm_1", "behavior": "allow"})
    );
}

#[test]
fn events_roundtrip_through_serde() {
    for event in sample_events() {
        let raw = serde_json::to_string(&event).unwrap();
        let back: SessionEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn append_then_replay_returns_events_in_order() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "abc");
    for event in sample_events() {
        log.append(&event).unwrap();
    }
    assert_eq!(log.replay(), sample_events());
    assert!(
        log_file(&tmp.0, "abc").exists(),
        "log not written to .sessions/<id>.jsonl"
    );
}

#[test]
fn replay_survives_a_process_restart() {
    let tmp = TmpDir::new();
    {
        let log = SessionLog::new(&tmp.0, "restart");
        log.append(&SessionEvent::TextDelta {
            text: "before".into(),
        })
        .unwrap();
    }
    // A fresh handle (new daemon) sees the same history and keeps appending.
    let log = SessionLog::new(&tmp.0, "restart");
    log.append(&SessionEvent::Done { usage: None }).unwrap();
    assert_eq!(
        log.replay(),
        vec![
            SessionEvent::TextDelta {
                text: "before".into()
            },
            SessionEvent::Done { usage: None },
        ]
    );
}

#[test]
fn tail_limits_by_event_count() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "count");
    for i in 0..10 {
        log.append(&SessionEvent::TextDelta {
            text: format!("t{i}"),
        })
        .unwrap();
    }
    let tail = log.tail(REPLAY_MAX_BYTES, 3);
    assert_eq!(
        tail,
        vec![
            SessionEvent::TextDelta { text: "t7".into() },
            SessionEvent::TextDelta { text: "t8".into() },
            SessionEvent::TextDelta { text: "t9".into() },
        ]
    );
}

#[test]
fn tail_limits_by_bytes_and_drops_the_partial_first_line() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "bytes");
    for i in 0..50 {
        log.append(&SessionEvent::TextDelta {
            text: format!("event number {i:04}"),
        })
        .unwrap();
    }
    let full_len = fs::metadata(log_file(&tmp.0, "bytes")).unwrap().len();
    let tail = log.tail(full_len / 2, REPLAY_MAX_EVENTS);
    assert!(!tail.is_empty());
    assert!(tail.len() < 50);
    // The last event is always intact; the truncated head never yields garbage.
    assert_eq!(
        tail.last(),
        Some(&SessionEvent::TextDelta {
            text: "event number 0049".into()
        })
    );
}

#[test]
fn tail_skips_corrupt_lines_and_missing_files() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "corrupt");
    assert_eq!(log.replay(), Vec::new());

    log.append(&SessionEvent::TextDelta { text: "ok".into() })
        .unwrap();
    let mut raw = fs::read_to_string(log_file(&tmp.0, "corrupt")).unwrap();
    raw.push_str("{not json\n");
    raw.push_str("{\"type\":\"unknown_variant\"}\n");
    fs::write(log_file(&tmp.0, "corrupt"), raw).unwrap();
    log.append(&SessionEvent::Done { usage: None }).unwrap();

    assert_eq!(
        log.replay(),
        vec![
            SessionEvent::TextDelta { text: "ok".into() },
            SessionEvent::Done { usage: None },
        ]
    );
}

#[test]
fn append_reports_an_unwritable_sessions_dir() {
    let tmp = TmpDir::new();
    // `.sessions` exists as a FILE: creating the directory must fail loudly.
    fs::write(tmp.0.join(".sessions"), "not a dir").unwrap();
    let log = SessionLog::new(&tmp.0, "blocked");
    let err = log.append(&SessionEvent::Done { usage: None }).unwrap_err();
    assert!(err.to_string().contains(".sessions"));
}

#[test]
fn tail_survives_invalid_utf8_regions() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "binary");
    log.append(&SessionEvent::TextDelta { text: "ok".into() })
        .unwrap();
    let path = log_file(&tmp.0, "binary");
    // A torn write injects raw bytes between two valid lines: only that line
    // is lost, never the replay around it.
    let mut raw = fs::read(&path).unwrap();
    raw.extend_from_slice(&[0xff, 0xfe, 0x00, 0x01]);
    raw.push(b'\n');
    fs::write(&path, raw).unwrap();
    log.append(&SessionEvent::Done { usage: None }).unwrap();
    assert_eq!(
        log.replay(),
        vec![
            SessionEvent::TextDelta { text: "ok".into() },
            SessionEvent::Done { usage: None },
        ]
    );
}

#[test]
fn migrate_moves_the_log_to_the_new_id() {
    let tmp = TmpDir::new();
    let log = SessionLog::new(&tmp.0, "requested");
    log.append(&SessionEvent::TextDelta { text: "one".into() })
        .unwrap();
    let migrated = log.migrate(&tmp.0, "reported");
    assert!(!log_file(&tmp.0, "requested").exists());
    assert_eq!(
        migrated.replay(),
        vec![SessionEvent::TextDelta { text: "one".into() }]
    );
}

#[test]
fn migrate_appends_after_an_existing_target_log() {
    let tmp = TmpDir::new();
    let existing = SessionLog::new(&tmp.0, "reported");
    existing
        .append(&SessionEvent::TextDelta { text: "old".into() })
        .unwrap();
    let log = SessionLog::new(&tmp.0, "requested");
    log.append(&SessionEvent::TextDelta { text: "new".into() })
        .unwrap();
    let migrated = log.migrate(&tmp.0, "reported");
    assert!(!log_file(&tmp.0, "requested").exists());
    assert_eq!(
        migrated.replay(),
        vec![
            SessionEvent::TextDelta { text: "old".into() },
            SessionEvent::TextDelta { text: "new".into() },
        ]
    );
}
