//! Daemon ⇄ client protocol. Serde messages over a stream (unix socket), with
//! length-prefixed framing (4-byte big-endian length + JSON payload).
//!
//! The daemon owns the state and exposes it as a `DomainSnapshot` (domain data,
//! no rendering); clients render it natively and send back `Intent`s. This
//! module is self-contained (serde only) so non-Rust clients can mirror it.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Single protocol version, bumped on any wire-format break. The daemon
/// rejects a `Hello` carrying a different version.
pub const PROTOCOL_VERSION: u32 = 1;

// --- handshake and telemetry ------------------------------------------------

/// Client device presented at handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Terminal,
    Mac,
    Iphone,
    Ipad,
    Other,
}

/// Daemon health exposed on every `Pong`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub uptime_ms: u64,
    pub snapshots_per_sec: f64,
    pub devices: Vec<DeviceStatus>,
}

/// A connected device as the daemon sees it. `rtt_ms` is the round-trip the
/// device itself measured and reported on its last `Ping`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub id: u64,
    pub name: String,
    pub kind: DeviceKind,
    pub rtt_ms: Option<u64>,
}

// --- client -> daemon ---------------------------------------------------------

/// Client → daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message of every connection. `token` is carried but not enforced yet.
    Hello {
        protocol_version: u32,
        device: Device,
        token: Option<String>,
    },
    /// A domain action (the only way a client mutates daemon state).
    Intent(Intent),
    /// Liveness probe. `last_rtt_ms` reports the round-trip measured on the
    /// previous ping, so the daemon can expose per-device RTT.
    Ping { seq: u64, last_rtt_ms: Option<u64> },
    /// The client finished running the `$EDITOR` requested via `RunEditor`.
    EditorDone,
    /// Shut the daemon down (stop sessions and exit).
    Shutdown,
}

/// What the text input is capturing (dispatched on submit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Defer,
    Convention,
    NewTask,
    NewTaskClaude,
    InitPath,
}

/// Domain intents. Names mirror the `App` actions they trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Intent {
    Quit,
    NextTab,
    SetTab { index: usize },
    SelectNext,
    SelectPrev,
    SelectFirst,
    SelectLast,
    FocusLeft,
    FocusRight,
    CardNext,
    CardPrev,
    OpenDetail,
    CloseDetail,
    DetailScrollDown,
    DetailScrollUp,
    DocsNext,
    DocsPrev,
    OpenDoc,
    CloseDoc,
    DocScrollDown,
    DocScrollUp,
    OpenPicker,
    ClosePicker,
    PickerNext,
    PickerPrev,
    PickerConfirm,
    DismissJob,
    JobScrollUp,
    JobScrollDown,
    JobScrollTop,
    JobFollow,
    ToggleZoom,
    Play,
    ReviewChat,
    Handoff,
    Finish,
    Ingest,
    AnalyzeParallel,
    StartSetup,
    EditConventions,
    StartInput { kind: InputKind, prefill: String },
    InputChar { ch: char },
    InputBackspace,
    InputCancel,
    InputSubmit,
    FinishSession,
    CloseSession,
}

// --- daemon -> client ---------------------------------------------------------

/// Daemon → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Handshake accepted; `device_id` identifies this connection in `DaemonStatus`.
    Welcome {
        protocol_version: u32,
        device_id: u64,
    },
    /// Handshake refused (version mismatch or missing `Hello`). Connection closes.
    Rejected { reason: String },
    /// Full domain state. Bursts of changes are coalesced into a single
    /// snapshot. Boxed: it dwarfs every other variant.
    Snapshot(Box<DomainSnapshot>),
    /// A live-session event (chat streaming; consumed by session panels).
    Session(SessionEvent),
    /// Liveness answer, carrying daemon telemetry.
    Pong { seq: u64, status: DaemonStatus },
    /// The daemon asks the client to run `$EDITOR` on this path (interactive step).
    RunEditor { path: String },
    /// The daemon asks the client to detach (e.g. the user quit).
    Detach,
}

/// Event of a live session (PTY lifecycle and output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// claude session UUID (stable across resume).
    pub session_id: String,
    pub kind: SessionEventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEventKind {
    Started {
        kind: SessionKind,
        task: Option<String>,
    },
    /// Raw PTY bytes (may split escape sequences at any point).
    Output {
        bytes: Vec<u8>,
    },
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Play,
    Review,
    Setup,
}

// --- domain snapshot ----------------------------------------------------------

/// Full domain state a client needs to render the board, docs and overlays.
/// The session panel (chat) is NOT here — it flows through `SessionEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainSnapshot {
    /// Current project name.
    pub project: String,
    /// All registered projects (project picker).
    pub projects: Vec<ProjectRef>,
    pub tab: TabId,
    pub board: BoardView,
    pub docs: DocsView,
    /// Project picker overlay (None = closed).
    pub picker: Option<PickerView>,
    /// Text input being captured (None = closed).
    pub input: Option<InputView>,
    /// Async job (ingest/capture/init/review/parallel) and its overlay.
    pub job: Option<JobView>,
    /// Whether the job overlay is visible (the job may keep running hidden).
    pub job_overlay: bool,
    /// Active toast text, already time-filtered by the daemon.
    pub toast: Option<String>,
    /// Footer status line.
    pub statusline: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub name: String,
    pub backlog: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabId {
    Board,
    Docs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusId {
    Tasks,
    Cards,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardView {
    pub tasks: Vec<TaskView>,
    /// Index into `tasks` (ignored while `project_selected`).
    pub selected: usize,
    /// The synthetic "· project" row (top of the list) is selected.
    pub project_selected: bool,
    pub focus: FocusId,
    /// Middle-column cards of the selected row.
    pub cards: Vec<CardView>,
    pub card_selected: usize,
    pub chat_fullscreen: bool,
    pub setup_needed: bool,
    /// A live setup session exists.
    pub setup_live: bool,
    pub detail_open: bool,
    pub detail_scroll: u16,
    /// Review report of the selected task (verdict card content).
    pub review: Option<ReviewView>,
    /// Repo overlaps between tasks: (task a, task b, repo).
    pub overlaps: Vec<OverlapView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlapView {
    pub a: String,
    pub b: String,
    pub repo: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusId {
    Backlog,
    Ready,
    Wip,
    Review,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskTypeId {
    Impl,
    Spike,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnforceId {
    Hook,
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintView {
    pub text: String,
    pub enforce: EnforceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrView {
    pub repo: String,
    pub pr: u64,
    pub branch: String,
    /// Head SHA the automatic review already ran against (idempotence marker).
    pub reviewed_sha: Option<String>,
}

/// Review badge of a task: `badge` = findings + unmet items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBadge {
    pub clean: bool,
    pub badge: usize,
    pub unmet: usize,
    /// When the capture ran, Unix seconds (clients render the "when").
    pub reviewed_at: Option<u64>,
}

/// Parallelism mark relative to the ACTIVE tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParallelMark {
    Conflict,
    Safe,
}

/// In-flight review state of a task: a capture running now, or a newer commit
/// still owing a review because its CI has not gone green yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewProgressId {
    Running,
    AwaitingCi,
    CiFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub task_type: TaskTypeId,
    pub status: StatusId,
    pub rfcs: Vec<String>,
    pub adrs: Vec<String>,
    pub prs: Vec<PrView>,
    pub deferred: Vec<String>,
    pub constraints: Vec<ConstraintView>,
    /// Markdown body (objective, acceptance criteria...).
    pub body: String,
    pub live_session: bool,
    pub review: Option<ReviewBadge>,
    pub parallel: Option<ParallelMark>,
    /// Automatic-review state (running / awaiting or blocked by CI).
    pub review_progress: Option<ReviewProgressId>,
}

/// Middle-column card of the selected row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardView {
    Session {
        kind: SessionKind,
        live: bool,
        /// Last activity, epoch milliseconds (clients render the age).
        last_activity_ms: u64,
    },
    Verdict {
        clean: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckVerdict {
    Pending,
    Ok,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckView {
    pub text: String,
    pub verdict: CheckVerdict,
}

/// Review report content for the selected task (right panel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewView {
    pub clean: bool,
    pub blocking: usize,
    /// Findings already rendered as display lines.
    pub findings: Vec<String>,
    pub constraints: Vec<CheckView>,
    pub criteria: Vec<CheckView>,
    /// Head SHAs the verdict was taken against (idempotence tag).
    pub reviewed_shas: Vec<String>,
    /// When the capture ran, Unix seconds.
    pub reviewed_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocsView {
    /// Docs root directory (shown when the list is empty).
    pub dir: String,
    /// Relative paths under the docs root.
    pub list: Vec<String>,
    pub selected: usize,
    /// Content of the selected doc (preview and expanded view).
    pub preview: String,
    pub doc_open: bool,
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerView {
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputView {
    pub kind: InputKind,
    pub buffer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub title: String,
    pub logs: Vec<String>,
    pub finished: bool,
    pub follow: bool,
    pub scroll: u16,
}

// --- framing --------------------------------------------------------------

/// Write a message with length-prefixed framing.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read a message (blocking). `Ok(None)` on clean EOF.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    let msg = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    Ok(Some(msg))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip_client_msg() {
        let msgs = vec![
            ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                device: Device {
                    name: "mbp".into(),
                    kind: DeviceKind::Terminal,
                },
                token: None,
            },
            ClientMsg::Intent(Intent::SelectNext),
            ClientMsg::Intent(Intent::StartInput {
                kind: InputKind::Defer,
                prefill: String::new(),
            }),
            ClientMsg::Ping {
                seq: 7,
                last_rtt_ms: Some(3),
            },
            ClientMsg::EditorDone,
            ClientMsg::Shutdown,
        ];
        let mut buf: Vec<u8> = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for expected in &msgs {
            let got: ClientMsg = read_msg(&mut cur).unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        // clean EOF
        let end: Option<ClientMsg> = read_msg(&mut cur).unwrap();
        assert!(end.is_none());
    }

    #[test]
    fn framing_roundtrip_server_msg() {
        let msgs = vec![
            ServerMsg::Welcome {
                protocol_version: PROTOCOL_VERSION,
                device_id: 3,
            },
            ServerMsg::Rejected {
                reason: "incompatible protocol".into(),
            },
            ServerMsg::Session(SessionEvent {
                session_id: "u-1".into(),
                kind: SessionEventKind::Output {
                    bytes: vec![0x1b, b'[', b'A'],
                },
            }),
            ServerMsg::Pong {
                seq: 7,
                status: DaemonStatus {
                    uptime_ms: 1000,
                    snapshots_per_sec: 2.5,
                    devices: vec![DeviceStatus {
                        id: 3,
                        name: "mbp".into(),
                        kind: DeviceKind::Terminal,
                        rtt_ms: Some(2),
                    }],
                },
            },
            ServerMsg::RunEditor {
                path: "/tmp/conventions.md".into(),
            },
            ServerMsg::Detach,
        ];
        let mut buf: Vec<u8> = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for expected in &msgs {
            let got: ServerMsg = read_msg(&mut cur).unwrap().unwrap();
            assert_eq!(&got, expected);
        }
    }

    #[test]
    fn read_msg_rejects_garbage_payload() {
        let mut buf = 4u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"zzzz");
        let mut cur = std::io::Cursor::new(buf);
        assert!(read_msg::<_, ClientMsg>(&mut cur).is_err());
    }

    #[test]
    fn snapshot_roundtrip_via_framing() {
        let snap = sample_snapshot();
        let mut buf = Vec::new();
        write_msg(&mut buf, &ServerMsg::Snapshot(Box::new(snap.clone()))).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: ServerMsg = read_msg(&mut cur).unwrap().unwrap();
        assert_eq!(got, ServerMsg::Snapshot(Box::new(snap)));
    }

    /// A snapshot exercising every view type (also mirrored in the golden
    /// fixtures, which pin the exact JSON).
    pub(crate) fn sample_snapshot() -> DomainSnapshot {
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
                    adrs: vec![],
                    prs: vec![PrView {
                        repo: "org/x".into(),
                        pr: 42,
                        branch: "feat/thing".into(),
                        reviewed_sha: Some("abc1234def".into()),
                    }],
                    deferred: vec!["later".into()],
                    constraints: vec![ConstraintView {
                        text: "do not touch src/legacy/".into(),
                        enforce: EnforceId::Hook,
                    }],
                    body: "## Objective\nx\n".into(),
                    live_session: true,
                    review: Some(ReviewBadge {
                        clean: false,
                        badge: 2,
                        unmet: 1,
                        reviewed_at: Some(1_700_000_000),
                    }),
                    parallel: Some(ParallelMark::Conflict),
                    review_progress: Some(ReviewProgressId::AwaitingCi),
                }],
                selected: 0,
                project_selected: false,
                focus: FocusId::Cards,
                cards: vec![
                    CardView::Session {
                        kind: SessionKind::Play,
                        live: true,
                        last_activity_ms: 1_700_000_000_000,
                    },
                    CardView::Verdict { clean: false },
                ],
                card_selected: 0,
                chat_fullscreen: false,
                setup_needed: true,
                setup_live: false,
                detail_open: false,
                detail_scroll: 0,
                review: Some(ReviewView {
                    clean: false,
                    blocking: 1,
                    findings: vec!["[MAJOR] src/a.rs:10 - broken".into()],
                    constraints: vec![CheckView {
                        text: "no legacy".into(),
                        verdict: CheckVerdict::Ok,
                    }],
                    criteria: vec![CheckView {
                        text: "roundtrip works".into(),
                        verdict: CheckVerdict::Pending,
                    }],
                    reviewed_shas: vec!["abc1234def".into()],
                    reviewed_at: Some(1_700_000_000),
                }),
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
                title: "ingest".into(),
                logs: vec!["scanning docs".into()],
                finished: false,
                follow: true,
                scroll: 0,
            }),
            job_overlay: true,
            toast: Some("play started on TASK-001".into()),
            statusline: "[Board] TASK-001 feat/thing".into(),
        }
    }
}
