//! Client of the TypeScript sidecar: spawns the node process, writes JSONL
//! commands on its stdin and demultiplexes the JSONL events from its stdout
//! by `request_id`. All access to claude goes through this bridge.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session_event::{ContentBlock, SessionEvent, Usage};

/// A regex the sidecar's tool guard applies to the tool target (command or
/// file path), derived from an `enforce: hook` constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardPattern {
    pub pattern: String,
    pub reason: String,
}

/// One interactive turn. Nullable fields are serialized explicitly (never
/// omitted): the TypeScript side reads them as `T | null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub request_id: String,
    /// jaum session uuid; forced as the claude session id on the first turn.
    pub session_id: String,
    /// claude session id to resume; takes precedence over `session_id`.
    pub resume: Option<String>,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub system_prompt_append: Option<String>,
    pub guard_patterns: Vec<GuardPattern>,
    pub content: Vec<ContentBlock>,
}

/// Verdict for a routed permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Commands written to the sidecar's stdin (one JSON per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    Chat(ChatTurn),
    PermissionResponse {
        permission_id: String,
        decision: PermissionDecision,
    },
    Abort {
        request_id: String,
    },
    Ping,
}

/// Events read from the sidecar's stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarEvent {
    Session {
        request_id: String,
        claude_session_id: String,
    },
    TextDelta {
        request_id: String,
        text: String,
    },
    ToolUse {
        request_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        request_id: String,
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    PermissionRequest {
        request_id: String,
        permission_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    Done {
        request_id: String,
        usage: Option<Usage>,
        stop_reason: Option<String>,
    },
    Error {
        request_id: String,
        category: String,
        message: String,
    },
    Pong,
}

impl SidecarEvent {
    /// Routing key: which in-flight request this event belongs to.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            SidecarEvent::Session { request_id, .. }
            | SidecarEvent::TextDelta { request_id, .. }
            | SidecarEvent::ToolUse { request_id, .. }
            | SidecarEvent::ToolResult { request_id, .. }
            | SidecarEvent::PermissionRequest { request_id, .. }
            | SidecarEvent::Done { request_id, .. }
            | SidecarEvent::Error { request_id, .. } => Some(request_id),
            SidecarEvent::Pong => None,
        }
    }

    /// Maps to the domain [`SessionEvent`] appended to the session log and
    /// broadcast to clients. `session` and `pong` are bookkeeping, not chat.
    pub fn to_session_event(&self) -> Option<SessionEvent> {
        match self {
            SidecarEvent::TextDelta { text, .. } => {
                Some(SessionEvent::TextDelta { text: text.clone() })
            }
            SidecarEvent::ToolUse {
                tool_use_id,
                name,
                input,
                ..
            } => Some(SessionEvent::ToolUse {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            SidecarEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some(SessionEvent::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            SidecarEvent::PermissionRequest {
                permission_id,
                tool_name,
                tool_input,
                ..
            } => Some(SessionEvent::PermissionRequest {
                permission_id: permission_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
            }),
            SidecarEvent::Done { usage, .. } => Some(SessionEvent::Done { usage: *usage }),
            SidecarEvent::Error {
                category, message, ..
            } => Some(SessionEvent::Error {
                category: category.clone(),
                message: message.clone(),
            }),
            SidecarEvent::Session { .. } | SidecarEvent::Pong => None,
        }
    }
}

/// Default deadline for a routed permission request: with no answer the
/// daemon denies and marks the session blocked.
pub const PERMISSION_TIMEOUT: Duration = Duration::from_secs(120);

/// A permission request waiting for a decision, owned by a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    pub session_id: String,
    pub permission_id: String,
    pub requested_at: Instant,
}

/// Tracks in-flight permission requests and expires the unanswered ones.
pub struct PermissionTracker {
    timeout: Duration,
    pending: Vec<PendingPermission>,
}

impl Default for PermissionTracker {
    fn default() -> Self {
        Self::new(PERMISSION_TIMEOUT)
    }
}

impl PermissionTracker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            pending: Vec::new(),
        }
    }

    pub fn track(&mut self, session_id: &str, permission_id: &str) {
        self.pending.push(PendingPermission {
            session_id: session_id.to_string(),
            permission_id: permission_id.to_string(),
            requested_at: Instant::now(),
        });
    }

    /// Removes and returns the requests past the deadline.
    pub fn expired(&mut self, now: Instant) -> Vec<PendingPermission> {
        let timeout = self.timeout;
        let (expired, pending) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| now.duration_since(p.requested_at) >= timeout);
        self.pending = pending;
        expired
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Resolves the sidecar bundle path: explicit override (the
/// `JAUM_SIDECAR_BUNDLE` env var, read by the caller) wins over the default
/// install location under the jaum home.
pub fn resolve_bundle(env_override: Option<&str>, jaum_home: &Path) -> PathBuf {
    match env_override {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => jaum_home.join("sidecar").join("jaum-sidecar.mjs"),
    }
}

type Router = Arc<Mutex<HashMap<String, Sender<SidecarEvent>>>>;

/// The spawned sidecar process and its JSONL plumbing. Dropping it kills the
/// process (sessions survive: the log and the claude session id are on disk).
pub struct SidecarClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    router: Router,
    pongs: Receiver<()>,
}

impl SidecarClient {
    /// Spawns `node <bundle>` and starts the reader threads. The Anthropic
    /// key variables are dropped so auth always rides on the claude CLI login.
    pub fn spawn(node_bin: &str, bundle: &Path) -> Result<Self> {
        let mut child = Command::new(node_bin)
            .arg(bundle)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning sidecar: {node_bin} {}", bundle.display()))?;

        let stdin = child.stdin.take().context("sidecar stdin unavailable")?;
        let stdout = child.stdout.take().context("sidecar stdout unavailable")?;
        let stderr = child.stderr.take().context("sidecar stderr unavailable")?;

        let router: Router = Arc::new(Mutex::new(HashMap::new()));
        let (pong_tx, pongs) = channel::<()>();

        let reader_router = router.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(event) = serde_json::from_str::<SidecarEvent>(&line) else {
                    // A malformed line is the sidecar's bug, never a reason
                    // to kill every session sharing the process.
                    eprintln!("[sidecar] dropping malformed event: {line}");
                    continue;
                };
                match event.request_id() {
                    Some(request_id) => {
                        let mut guard = reader_router.lock().unwrap();
                        let dead = match guard.get(request_id) {
                            Some(tx) => tx.send(event.clone()).is_err(),
                            None => false,
                        };
                        if dead {
                            guard.remove(event.request_id().unwrap());
                        }
                    }
                    None => {
                        let _ = pong_tx.send(());
                    }
                }
            }
        });

        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("[sidecar] {line}");
            }
        });

        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            router,
            pongs,
        })
    }

    /// Writes one command as a JSONL line.
    pub fn send(&self, cmd: &SidecarCommand) -> Result<()> {
        let mut line = serde_json::to_string(cmd).context("serializing sidecar command")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().unwrap();
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .context("writing to the sidecar stdin")
    }

    /// Registers a receiver for a request's events and sends the turn.
    pub fn chat(&self, turn: ChatTurn) -> Result<Receiver<SidecarEvent>> {
        let (tx, rx) = channel();
        self.router
            .lock()
            .unwrap()
            .insert(turn.request_id.clone(), tx);
        if let Err(e) = self.send(&SidecarCommand::Chat(turn.clone())) {
            self.router.lock().unwrap().remove(&turn.request_id);
            return Err(e);
        }
        Ok(rx)
    }

    /// Drops the routing entry of a finished request.
    pub fn unregister(&self, request_id: &str) {
        self.router.lock().unwrap().remove(request_id);
    }

    pub fn abort(&self, request_id: &str) -> Result<()> {
        self.send(&SidecarCommand::Abort {
            request_id: request_id.to_string(),
        })
    }

    pub fn respond_permission(
        &self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.send(&SidecarCommand::PermissionResponse {
            permission_id: permission_id.to_string(),
            decision,
        })
    }

    pub fn ping(&self) -> Result<()> {
        self.send(&SidecarCommand::Ping)
    }

    /// Consumes and reports whether at least one pong arrived since the last
    /// call (health signal for the supervisor).
    pub fn pong_seen(&self) -> bool {
        let mut seen = false;
        while self.pongs.try_recv().is_ok() {
            seen = true;
        }
        seen
    }

    /// `false` once the process exited (the supervisor respawns lazily).
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SidecarClient {
    fn drop(&mut self) {
        self.kill();
    }
}

// In-crate unit tests so llvm-cov attributes the exercised lines to this file
// (integration-test includes are dropped by the coverage path filter). The
// module is crate-visible because the app tests reuse its sidecar stub.
#[cfg(test)]
#[path = "sidecar_tests.rs"]
pub(crate) mod sidecar_tests;
