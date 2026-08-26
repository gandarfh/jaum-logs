//! Domain-level chat events of a session and their per-session append-only
//! log. The daemon accumulates every event here so a client attaching later
//! replays the conversation; retention is handled by the finish flow, never
//! by a GC.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A user-visible content block (text or inline base64 image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { source: ImageSource },
}

/// Inline image payload, mirroring the API's base64 source shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

/// Token usage of a finished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

/// One chat event of a session, broadcast to clients and appended to the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TextDelta {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
    PermissionRequest {
        permission_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    /// Resolution of a routed permission request (allow or deny), so a
    /// replayed log never shows a request as eternally pending.
    PermissionDecision {
        permission_id: String,
        behavior: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Done {
        usage: Option<Usage>,
    },
    Error {
        category: String,
        message: String,
    },
}

/// Replay budget on attach: whatever comes last within these limits is sent;
/// older history stays in the file for on-demand scrollback.
pub const REPLAY_MAX_BYTES: u64 = 1024 * 1024;
pub const REPLAY_MAX_EVENTS: usize = 2000;

/// Append-only JSONL log of a session under `<home>/.sessions/<id>.jsonl`.
pub struct SessionLog {
    dir: PathBuf,
    path: PathBuf,
    /// `.sessions/` was already created by this handle (one syscall per
    /// event otherwise; long turns append thousands of lines).
    dir_ready: std::sync::atomic::AtomicBool,
}

impl SessionLog {
    /// `home` is the project's external dir (`~/jaum/<project>`).
    pub fn new(home: &Path, session_id: &str) -> Self {
        let dir = home.join(".sessions");
        let path = dir.join(format!("{session_id}.jsonl"));
        Self {
            dir,
            path,
            dir_ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Appends one event as a line, creating `.sessions/` on first write.
    pub fn append(&self, event: &SessionEvent) -> Result<()> {
        use std::sync::atomic::Ordering;
        if !self.dir_ready.load(Ordering::Relaxed) {
            fs::create_dir_all(&self.dir)
                .with_context(|| format!("creating {}", self.dir.display()))?;
            self.dir_ready.store(true, Ordering::Relaxed);
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut line = serde_json::to_string(event).context("serializing session event")?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", self.path.display()))
    }

    /// Moves the log to a new session id and returns the handle for it.
    /// Used when the SDK reports a claude session id different from the one
    /// requested: the history must follow the resumable identifier or the
    /// next restart replays nothing.
    pub fn migrate(self, home: &Path, new_id: &str) -> SessionLog {
        let new = SessionLog::new(home, new_id);
        if self.path.exists() {
            let _ = fs::create_dir_all(&new.dir);
            if new.path.exists() {
                // Never clobber an existing log for the target id: its events
                // predate this run's, so the migrated lines go after them.
                let appended = fs::read(&self.path).and_then(|bytes| {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&new.path)
                        .and_then(|mut f| f.write_all(&bytes))
                });
                if appended.is_ok() {
                    let _ = fs::remove_file(&self.path);
                }
            } else {
                let _ = fs::rename(&self.path, &new.path);
            }
        }
        new
    }

    /// Replays the tail of the log within the default attach budget.
    pub fn replay(&self) -> Vec<SessionEvent> {
        self.tail(REPLAY_MAX_BYTES, REPLAY_MAX_EVENTS)
    }

    /// Last events within `max_bytes`/`max_events`. Missing file means no
    /// history; unparseable lines (partial writes, format drift) are skipped
    /// so one bad line never loses the whole replay.
    pub fn tail(&self, max_bytes: u64, max_events: usize) -> Vec<SessionEvent> {
        let Ok(mut file) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut skip_first = false;
        // A failed seek degrades to reading the whole file instead of losing
        // the replay. The cut likely lands mid-line; the first partial line
        // is noise.
        if len > max_bytes && file.seek(SeekFrom::Start(len - max_bytes)).is_ok() {
            skip_first = true;
        }
        let mut raw = Vec::new();
        if file.read_to_end(&mut raw).is_err() {
            return Vec::new();
        }
        // Lossy decoding: an invalid UTF-8 region (torn write, disk noise)
        // corrupts only its own line, never the whole replay.
        let raw = String::from_utf8_lossy(&raw);
        let mut events: Vec<SessionEvent> = raw
            .lines()
            .skip(usize::from(skip_first))
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if events.len() > max_events {
            events.drain(..events.len() - max_events);
        }
        events
    }
}

// In-crate unit tests so llvm-cov attributes the exercised lines to this file
// (integration-test includes are dropped by the coverage path filter).
#[cfg(test)]
#[path = "session_event_tests.rs"]
mod session_event_tests;
