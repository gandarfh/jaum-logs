//! Pure TUI state and logic (testable without a terminal). Rendering lives in `tui`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jaum_adapters::{ClaudeExecutor, Gh, Git, Session};
use serde::{Deserialize, Serialize};

use crate::config::Config as GlobalConfig;
use crate::protocol::{Intent, SessionEvent, SessionEventKind};
use crate::session_event::{ContentBlock, SessionEvent as ChatEvent, SessionLog};
use crate::sidecar::{
    ChatTurn, GuardPattern, PermissionDecision, PermissionTracker, SidecarClient, SidecarEvent,
    resolve_bundle,
};
use jaum_core::{CiStatus, MergeState, PrCi, Repo, Status, Store, Task};
use jaum_flows::conflict::Conflict;
use jaum_flows::finish::Finish;
use jaum_flows::ingest::Ingest;
use jaum_flows::parallel::{Parallel, ParallelReport};
use jaum_flows::play::{GuardSpec, Play, PlayLaunch};
use jaum_flows::review::{Review, ReviewReport};
use jaum_flows::setup::{Setup, branch_leaks_id, is_template};

/// What's missing from the project's mandatory setup (validated at init/open).
/// jaum (not the prompt) enforces these invariants.
#[derive(Debug, Default)]
pub struct SetupNeeds {
    /// Backlog tasks with no linked repo/branch.
    pub unlinked: Vec<String>,
    /// Tasks whose branch leaks jaum's internal id (e.g. `feat/task-001`).
    pub leaky_branches: Vec<String>,
    /// `conventions.md` still template/empty.
    pub conventions_template: bool,
    /// `setup.md` (repo↔area mapping) missing.
    pub mapping_missing: bool,
}

impl SetupNeeds {
    pub fn any(&self) -> bool {
        !self.unlinked.is_empty()
            || !self.leaky_branches.is_empty()
            || self.conventions_template
            || self.mapping_missing
    }
}

/// TUI tabs. Sessions and review live inside the Board (per task), not as tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Board,
    Docs,
}

impl Tab {
    pub fn all() -> [Tab; 2] {
        [Tab::Board, Tab::Docs]
    }
    pub fn index(self) -> usize {
        Tab::all().iter().position(|t| *t == self).unwrap()
    }
    pub fn from_index(i: usize) -> Tab {
        Tab::all()[i.min(1)]
    }
    pub fn next(self) -> Tab {
        Tab::from_index((self.index() + 1) % 2)
    }
}

/// Focused panel in the Board (3-column layout: tasks | cards | chat).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardFocus {
    Tasks,
    Cards,
    Chat,
}

/// Middle-column item of a task: a session (index into `sessions`) or the
/// review verdict card (`.review.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardCard {
    Session(usize),
    Verdict,
}

/// In-flight/pending state of a task's automatic review, for the board glyphs.
/// The persisted verdict badge (`✓`/`⚑`) covers the "done" case; this covers
/// what is still owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewProgress {
    /// Capture running right now.
    Running,
    /// A new commit exists whose CI is still running; re-review is due once green.
    AwaitingCi,
    /// A new commit exists whose CI is red; the review stays blocked until it passes.
    CiFailed,
}

/// Canonical display order of statuses on the board.
pub const STATUS_ORDER: [Status; 5] = [
    Status::Wip,
    Status::Review,
    Status::Ready,
    Status::Backlog,
    Status::Merged,
];

fn status_rank(s: Status) -> usize {
    STATUS_ORDER
        .iter()
        .position(|x| *x == s)
        .unwrap_or(usize::MAX)
}

/// Sorts tasks for the board: by status (canonical order), then by id.
pub fn sort_for_board(mut tasks: Vec<Task>) -> Vec<Task> {
    tasks.sort_by(|a, b| {
        status_rank(a.status)
            .cmp(&status_rank(b.status))
            .then_with(|| a.id.cmp(&b.id))
    });
    tasks
}

/// Recursively lists the relative paths of every `.md` under `dir`.
pub fn list_docs(dir: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().is_some_and(|e| e == "md")
                && let Ok(rel) = p.strip_prefix(base)
            {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Kind of a live session (label + color in the list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Play,
    Review,
    Setup,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::Play => "play",
            SessionKind::Review => "review",
            SessionKind::Setup => "setup",
        }
    }
}

/// Epoch milliseconds of a `SystemTime` (0 if before the epoch).
fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// `SystemTime` from epoch milliseconds.
fn from_epoch_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Persisted session record (survives daemon shutdown). Holds no PTY — just
/// enough to resume (`claude --resume`) or show as history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub kind: SessionKind,
    pub task: Option<String>,
    /// claude session UUID (forced on the first turn; resumed afterwards).
    pub claude_session_id: String,
    /// origin cwd (the play worktree, the review repo, the setup home).
    pub cwd: PathBuf,
    pub worktrees: Vec<(String, PathBuf)>,
    pub created_ms: u64,
    pub last_activity_ms: u64,
    pub finished: bool,
    /// A permission request expired unanswered; the session needs attention.
    #[serde(default)]
    pub blocked: bool,
}

/// Chat state of a sidecar-backed session (play). The log is the source of
/// truth for replay; `rx` exists only while a turn is in flight.
pub struct ChatState {
    /// Events of the active turn (None between turns).
    pub rx: Option<Receiver<SidecarEvent>>,
    /// request_id of the active turn.
    pub request_id: Option<String>,
    /// Monotonic turn counter (builds deterministic request ids).
    pub turn_seq: u64,
    /// Messages waiting for the active turn to finish (e.g. handoff).
    pub queued: VecDeque<String>,
    /// Append-only event log (`.sessions/<id>.jsonl`).
    pub log: SessionLog,
}

/// A session and its metadata, to run many in parallel. PTY sessions
/// (review/setup) have `session`/`rx` and a vt100 parser fed by a reader
/// thread; sidecar sessions (play) have `chat` instead; history entries
/// (restored from disk and not resumable) have neither live handle.
pub struct SessionEntry {
    pub kind: SessionKind,
    /// Linked task (None for setup).
    pub task: Option<String>,
    pub session: Option<Session>,
    pub parser: vt100::Parser,
    pub rx: Option<Receiver<Vec<u8>>>,
    /// Sidecar-backed chat (play sessions).
    pub chat: Option<ChatState>,
    /// Worktrees to clean up on close (play only).
    pub worktrees: Vec<(String, PathBuf)>,
    /// claude session UUID, to resume after restart.
    pub claude_session_id: String,
    /// origin cwd (kept so resume can find the conversation).
    pub cwd: PathBuf,
    pub created: SystemTime,
    pub last_activity: SystemTime,
    pub finished: bool,
    /// A permission request expired unanswered (cleared on the next turn).
    pub blocked: bool,
    /// Monotonic creation sequence. Breaks ties in the activity ordering
    /// (newest on top) when timestamps coincide.
    pub seq: u64,
}

/// Global session-creation counter (deterministic tiebreak for ordering).
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn next_session_seq() -> u64 {
    SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl SessionEntry {
    /// Creates the LIVE entry and spawns the thread that pumps the PTY into the parser.
    fn spawn(
        kind: SessionKind,
        task: Option<String>,
        session: Session,
        worktrees: Vec<(String, PathBuf)>,
        claude_session_id: String,
        cwd: PathBuf,
    ) -> Self {
        use std::sync::mpsc::channel;
        use std::thread;

        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        let (tx, rx) = channel::<Vec<u8>>();
        let mut rx = Some(rx);
        if let Ok(mut reader) = session.reader() {
            thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        } else {
            rx = None;
        }
        let now = SystemTime::now();
        Self {
            kind,
            task,
            session: Some(session),
            parser,
            rx,
            chat: None,
            worktrees,
            claude_session_id,
            cwd,
            created: now,
            last_activity: now,
            finished: false,
            blocked: false,
            seq: next_session_seq(),
        }
    }

    /// Sidecar-backed entry (play): no PTY, events arrive per turn via the
    /// chat receiver and accumulate in the session log.
    pub(crate) fn sidecar(
        kind: SessionKind,
        task: Option<String>,
        worktrees: Vec<(String, PathBuf)>,
        session_id: String,
        cwd: PathBuf,
        log: SessionLog,
    ) -> Self {
        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        let now = SystemTime::now();
        Self {
            kind,
            task,
            session: None,
            parser,
            rx: None,
            chat: Some(ChatState {
                rx: None,
                request_id: None,
                turn_seq: 0,
                queued: VecDeque::new(),
                log,
            }),
            worktrees,
            claude_session_id: session_id,
            cwd,
            created: now,
            last_activity: now,
            finished: false,
            blocked: false,
            seq: next_session_seq(),
        }
    }

    /// HISTORY entry (no PTY, no chat): restored from disk when the session
    /// isn't resumable (finished, cwd gone, or resume failed).
    fn history(rec: &SessionRecord) -> Self {
        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        Self {
            kind: rec.kind,
            task: rec.task.clone(),
            session: None,
            parser,
            rx: None,
            chat: None,
            worktrees: rec.worktrees.clone(),
            claude_session_id: rec.claude_session_id.clone(),
            cwd: rec.cwd.clone(),
            created: from_epoch_ms(rec.created_ms),
            last_activity: from_epoch_ms(rec.last_activity_ms),
            finished: true,
            blocked: rec.blocked,
            seq: next_session_seq(),
        }
    }

    /// Serializable snapshot of the session.
    fn to_record(&self) -> SessionRecord {
        SessionRecord {
            kind: self.kind,
            task: self.task.clone(),
            claude_session_id: self.claude_session_id.clone(),
            cwd: self.cwd.clone(),
            worktrees: self.worktrees.clone(),
            created_ms: epoch_ms(self.created),
            last_activity_ms: epoch_ms(self.last_activity),
            finished: self.finished,
            blocked: self.blocked,
        }
    }

    /// Live? PTY sessions: the process is running. Sidecar sessions: not
    /// finished (they stay resumable between turns, without a process).
    pub fn is_live(&self) -> bool {
        (self.session.is_some() || self.chat.is_some()) && !self.finished
    }

    /// `true` while a sidecar turn is in flight (a query is running).
    pub fn turn_active(&self) -> bool {
        self.chat.as_ref().is_some_and(|c| c.rx.is_some())
    }

    /// Renders one chat event into the vt100 parser (plain-text view of the
    /// conversation until the structured chat panel exists).
    fn render_event(&mut self, event: &ChatEvent) {
        let text = match event {
            ChatEvent::TextDelta { text } => text.replace('\n', "\r\n"),
            ChatEvent::ToolUse { name, input, .. } => {
                let mut summary = input.to_string();
                summary.truncate(120);
                format!("\r\n[tool {name}] {summary}\r\n")
            }
            ChatEvent::ToolResult {
                content, is_error, ..
            } => {
                let mut first = content
                    .first()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.clone(),
                        ContentBlock::Image { .. } => "[image]".into(),
                    })
                    .unwrap_or_default();
                first.truncate(120);
                let tag = if *is_error { "result error" } else { "result" };
                format!("[{tag}] {}\r\n", first.replace('\n', " "))
            }
            ChatEvent::Image { .. } => "[image]\r\n".into(),
            ChatEvent::PermissionRequest { tool_name, .. } => {
                format!("[permission? {tool_name}]\r\n")
            }
            ChatEvent::Done { .. } => "\r\n[turn done]\r\n".into(),
            ChatEvent::Error { category, message } => {
                format!("\r\n[error {category}] {message}\r\n")
            }
        };
        self.parser.process(text.as_bytes());
    }

    /// Name shown in the list: `play · TASK-001`, `setup`, etc.
    pub fn name(&self) -> String {
        match &self.task {
            Some(t) => format!("{} · {t}", self.kind.label()),
            None => self.kind.label().to_string(),
        }
    }

    /// Drains the PTY's pending bytes; marks `finished` on EOF. No-op for history
    /// entries (no `rx`). Returns the drained bytes and whether the session
    /// finished during this drain (both feed `SessionEvent`s).
    fn drain(&mut self) -> (Vec<u8>, bool) {
        use std::sync::mpsc::TryRecvError;
        let was_finished = self.finished;
        let mut drained = Vec::new();
        let Some(rx) = &self.rx else {
            return (drained, false);
        };
        loop {
            match rx.try_recv() {
                Ok(bytes) => {
                    self.parser.process(&bytes);
                    drained.extend_from_slice(&bytes);
                    self.last_activity = SystemTime::now();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    break;
                }
            }
        }
        // proactive detection: the process may exit (Ctrl+C/Ctrl+D in claude)
        // before the channel EOF propagates. `try_wait` on the child is authoritative.
        if !self.finished
            && let Some(s) = &mut self.session
            && matches!(s.try_wait(), Ok(Some(_)))
        {
            self.finished = true;
        }
        (drained, self.finished && !was_finished)
    }
}

/// What the text input is capturing (dispatched on Enter). Shared with the
/// wire protocol so intents carry it as-is.
pub use crate::protocol::InputKind;

/// Async job kind (defines what to do when it finishes).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Ingest,
    Capture,
    Init,
    Review,
    Parallel,
}

/// Message from a background job to the UI.
pub enum JobMsg {
    /// Live log line.
    Log(String),
    /// Termination. `Ok` = project name (Init) or success phrase; `Err` =
    /// failure message.
    Done(Result<String, String>),
}

/// An async job (ingest/capture/init) with its live logs.
pub struct Job {
    pub kind: JobKind,
    pub title: String,
    /// Task the job acts on, when it has one (review). Drives the in-flight
    /// indicators (statusline + list glyph) while it runs.
    pub task: Option<String>,
    pub logs: Vec<String>,
    pub rx: Receiver<JobMsg>,
    pub finished: bool,
    /// Scroll offset (lines, post-wrap). Only used when `follow` is false.
    pub scroll: u16,
    /// Follows the live tail; turns off when the user scrolls up to read.
    pub follow: bool,
}

/// Expands `~`/`~/...` to HOME (the TUI takes user-typed paths).
fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

const HINT: &str = "jaum";

/// Converts the flow's guard spec into wire guard patterns for the sidecar.
fn guard_patterns(spec: &GuardSpec) -> Vec<GuardPattern> {
    spec.guard_patterns
        .iter()
        .map(|g| GuardPattern {
            pattern: g.pattern.clone(),
            reason: g.reason.clone(),
        })
        .collect()
}

/// Full application state.
pub struct App {
    pub git: Git,
    pub gh: Gh,
    pub executor: ClaudeExecutor,

    /// Global config (all projects) and the current project index.
    pub config: GlobalConfig,
    pub current: usize,
    /// Derived from the current project:
    pub store: Store,
    pub repos: HashMap<String, PathBuf>,
    pub docs_dir: PathBuf,
    pub work_dir: PathBuf,
    /// Project conventions (contents of conventions.md) + path.
    pub conventions: String,
    pub conventions_path: PathBuf,

    pub tab: Tab,
    pub tasks: Vec<Task>,
    pub selected: usize,
    pub overlaps: Vec<(String, String, String)>,
    /// Message from the last interaction. Not shown in the footer: becomes a temporary toast.
    pub status_msg: String,
    /// When the current toast started (None = none) and the text already shown.
    pub toast_started: Option<Instant>,
    pub toast_shown: String,
    /// Last reload of on-disk state (backlog/conventions), to pick up external
    /// edits — e.g. what the setup session just wrote.
    pub last_reload: Instant,
    /// File watcher for the project folder: triggers an immediate reload when a
    /// session (setup/play) writes something. `watcher` is kept only to stay alive.
    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<Receiver<()>>,
    pub should_quit: bool,

    /// Live sessions (PTYs), running in parallel. The "focused" session is the one
    /// of the selected card in the Board (`current_session_idx`).
    pub sessions: Vec<SessionEntry>,

    /// Session lifecycle/output events accumulated since the last take (the
    /// daemon broadcasts them; the local TUI discards them each frame).
    session_events: Vec<SessionEvent>,

    pub review_report: Option<ReviewReport>,

    /// Board (3-column layout): focused panel, cards-column cursor, and whether the
    /// chat is fullscreen. `project_selected` = the synthetic "· project" row (top
    /// of the list) is selected instead of a task; its cards are the setup sessions.
    pub board_focus: BoardFocus,
    pub card_selected: usize,
    pub chat_fullscreen: bool,
    pub project_selected: bool,

    /// Last parallelism analysis (loaded from `work_dir/parallel.json`).
    pub parallel: Option<ParallelReport>,

    /// Session tab: pending prefix (Ctrl+B) for the next jaum command.
    pub pending_prefix: bool,
    /// `Some((kind, buffer))` while capturing text (defer/convention/task).
    pub input: Option<(InputKind, String)>,

    /// Project-switch overlay.
    pub project_picker: bool,
    pub picker_selected: usize,

    /// Detail overlay for the selected task.
    pub detail_open: bool,
    /// Vertical scroll of the detail.
    pub detail_scroll: u16,

    /// Docs (relative paths under docs_dir) and navigation/viewing.
    pub docs: Vec<String>,
    pub docs_selected: usize,
    pub doc_open: bool,
    pub doc_scroll: u16,

    /// Request to open `conventions.md` in `$EDITOR` (handled in the event loop).
    pub edit_request: bool,

    /// In-flight async job (ingest/capture/init) with live logs.
    pub job: Option<Job>,
    /// Logs overlay visible (separate from the job: the job keeps running in background).
    pub job_overlay: bool,

    /// Background PR sync: last pass and an "in progress" flag (avoids overlapping
    /// threads). While a play session is live, discovers the PR number opened by the
    /// agent and writes it to the task.
    last_pr_sync: Instant,
    pr_sync_running: Arc<AtomicBool>,

    /// CI watch: polls the PRs of eligible tasks and, when every check is green,
    /// dispatches the structured review capture automatically. Observations come
    /// back on `ci_rx`; `ci_tx` is cloned into the polling thread.
    last_ci_poll: Instant,
    ci_poll_interval: Duration,
    ci_poll_running: Arc<AtomicBool>,
    ci_tx: Sender<(String, Vec<(Repo, PrCi)>)>,
    ci_rx: Receiver<(String, Vec<(Repo, PrCi)>)>,
    /// Latest CI observation per task id, kept so the board can show whether a
    /// re-review is still pending (new commit whose CI is running or red).
    pub(crate) ci_obs: HashMap<String, Vec<(Repo, PrCi)>>,

    /// Sidecar process (lazy; respawned when it dies).
    sidecar: Option<SidecarClient>,
    /// Last health ping: `None` = no ping in flight; `Some` = waiting for the
    /// pong (a second interval without one kills the hung process).
    sidecar_pinged: Option<Instant>,
    last_sidecar_ping: Instant,
    /// In-flight permission requests waiting for a decision.
    pub permissions: PermissionTracker,
    /// When true, permission requests wait for a client decision (denied by
    /// default after the timeout). While no client can answer (the structured
    /// chat panel is pending), the daemon routes the request into the log and
    /// auto-allows: the mechanical guards already ran in the sidecar.
    pub route_permissions: bool,

    /// Binaries used by background jobs (their threads build their own adapters);
    /// tests point these at stubs so no real `claude`/`gh` is ever spawned.
    claude_bin: String,
    gh_bin: String,
    /// node binary + bundle used to spawn the sidecar (stubbed in tests).
    node_bin: String,
    sidecar_bundle: PathBuf,
    /// Global-config access used by the init job; injectable so tests never touch
    /// the user's `~/jaum`.
    load_config: fn() -> Result<GlobalConfig>,
    init_project: fn(&std::path::Path, &[PathBuf]) -> Result<crate::config::Project>,
}

impl App {
    /// Creates the App from the global config, opening project `current`.
    pub fn new(config: GlobalConfig, current: usize) -> Result<Self> {
        let project = config
            .projects
            .get(current)
            .context("invalid project index")?;
        let conventions_path = project.conventions_path();
        let conventions = std::fs::read_to_string(&conventions_path).unwrap_or_default();
        let ci_poll_interval = Duration::from_secs(config.ci_poll_secs.unwrap_or(30));
        let (ci_tx, ci_rx) = channel();
        let mut app = Self {
            git: Git::new(),
            gh: Gh::new(),
            executor: ClaudeExecutor::new(),
            store: Store::new(&project.backlog),
            repos: project.repo_map(),
            docs_dir: project.docs.clone(),
            work_dir: project.work_dir.clone(),
            conventions,
            conventions_path,
            current,
            config,
            tab: Tab::Board,
            tasks: Vec::new(),
            selected: 0,
            overlaps: Vec::new(),
            status_msg: HINT.into(),
            toast_started: None,
            toast_shown: HINT.into(),
            last_reload: Instant::now(),
            watcher: None,
            watch_rx: None,
            should_quit: false,
            sessions: Vec::new(),
            session_events: Vec::new(),
            review_report: None,
            board_focus: BoardFocus::Tasks,
            card_selected: 0,
            chat_fullscreen: false,
            project_selected: false,
            parallel: None,
            pending_prefix: false,
            input: None,
            project_picker: false,
            picker_selected: 0,
            detail_open: false,
            detail_scroll: 0,
            docs: Vec::new(),
            docs_selected: 0,
            doc_open: false,
            doc_scroll: 0,
            edit_request: false,
            job: None,
            job_overlay: false,
            last_pr_sync: Instant::now(),
            pr_sync_running: Arc::new(AtomicBool::new(false)),
            last_ci_poll: Instant::now(),
            ci_poll_interval,
            ci_poll_running: Arc::new(AtomicBool::new(false)),
            ci_tx,
            ci_rx,
            ci_obs: HashMap::new(),
            sidecar: None,
            sidecar_pinged: None,
            last_sidecar_ping: Instant::now(),
            permissions: PermissionTracker::default(),
            route_permissions: false,
            claude_bin: "claude".into(),
            gh_bin: "gh".into(),
            node_bin: "node".into(),
            sidecar_bundle: resolve_bundle(
                std::env::var("JAUM_SIDECAR_BUNDLE").ok().as_deref(),
                &crate::config::jaum_home().unwrap_or_default(),
            ),
            load_config: GlobalConfig::load,
            init_project: crate::config::init_project,
        };
        app.refresh()?;
        app.rehydrate_sessions();
        app.start_watch();
        Ok(app)
    }

    /// Rehydrates persisted sessions at boot: live ones (not finished, worktree/cwd
    /// still on disk) come back resumed via `claude --resume`; the rest enter as
    /// history (no PTY). Never breaks boot.
    pub(crate) fn rehydrate_sessions(&mut self) {
        for rec in self.load_session_records() {
            let entry = self.rehydrate_one(rec);
            self.sessions.push(entry);
        }
    }

    /// Builds a `SessionEntry` from a record: resumes when possible, otherwise
    /// becomes history.
    fn rehydrate_one(&self, rec: SessionRecord) -> SessionEntry {
        // not resumable: already finished, or the cwd (worktree/repo) is gone.
        if rec.finished || !rec.cwd.exists() {
            return self.with_replayed_log(SessionEntry::history(&rec));
        }
        // Play is sidecar-backed: no process to relaunch. The entry comes back
        // resumable (next turn resumes the claude session) with the log replayed.
        if rec.kind == SessionKind::Play {
            if rec.task.is_none() {
                return self.with_replayed_log(SessionEntry::history(&rec));
            }
            let log = SessionLog::new(&self.home(), &rec.claude_session_id);
            let mut e = SessionEntry::sidecar(
                rec.kind,
                rec.task.clone(),
                rec.worktrees.clone(),
                rec.claude_session_id.clone(),
                rec.cwd.clone(),
                log,
            );
            e.created = from_epoch_ms(rec.created_ms);
            e.last_activity = from_epoch_ms(rec.last_activity_ms);
            e.blocked = rec.blocked;
            return self.with_replayed_log(e);
        }
        match self.resume_session(&rec) {
            Ok(session) => {
                let mut e = SessionEntry::spawn(
                    rec.kind,
                    rec.task.clone(),
                    session,
                    rec.worktrees.clone(),
                    rec.claude_session_id.clone(),
                    rec.cwd.clone(),
                );
                // preserve original timestamps (don't reset the clock on resume).
                e.created = from_epoch_ms(rec.created_ms);
                e.last_activity = from_epoch_ms(rec.last_activity_ms);
                e
            }
            // resume failed (invalid cwd, claude unavailable): fall back to history.
            Err(_) => SessionEntry::history(&rec),
        }
    }

    /// Replays the persisted event log into the entry's text view (attach
    /// replay: whoever comes back sees the conversation so far).
    fn with_replayed_log(&self, mut e: SessionEntry) -> SessionEntry {
        for event in SessionLog::new(&self.home(), &e.claude_session_id).replay() {
            e.render_event(&event);
        }
        e
    }

    /// Relaunches claude with `--resume` for the record, per kind (PTY kinds:
    /// review and setup migrate to the sidecar in a later phase).
    fn resume_session(&self, rec: &SessionRecord) -> Result<Session> {
        match rec.kind {
            SessionKind::Play => unreachable!("play resumes over the sidecar"),
            SessionKind::Review => {
                let id = rec.task.as_deref().context("review record without task")?;
                Review::new(
                    &self.store,
                    &self.git,
                    &self.gh,
                    &self.executor,
                    &self.docs_dir,
                    self.repos.clone(),
                    self.conventions.clone(),
                )
                .resume(id, &rec.claude_session_id, &rec.cwd)
            }
            SessionKind::Setup => Setup::new(
                &self.store,
                &self.executor,
                self.home(),
                self.repos.clone(),
                self.conventions.clone(),
            )
            .resume(&rec.claude_session_id),
        }
    }

    /// (Re)starts the file watcher on the current project folder. Any write there
    /// (setup/play editing backlog/docs/conventions) triggers an immediate reload.
    fn start_watch(&mut self) {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = channel::<()>();
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        }) {
            Ok(mut w) => {
                let _ = w.watch(&self.home(), RecursiveMode::Recursive);
                self.watcher = Some(w);
                self.watch_rx = Some(rx);
            }
            Err(_) => {
                self.watcher = None;
                self.watch_rx = None;
            }
        }
    }

    pub fn request_edit_conventions(&mut self) {
        self.edit_request = true;
    }

    pub fn reload_conventions(&mut self) {
        self.conventions = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
    }

    // --- task detail -------------------------------------------------------

    pub fn open_detail(&mut self) {
        if self.selected_task().is_some() {
            self.detail_open = true;
            self.detail_scroll = 0;
        }
    }
    pub fn close_detail(&mut self) {
        self.detail_open = false;
    }
    pub fn detail_scroll_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(1);
    }
    pub fn detail_scroll_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    pub fn project_name(&self) -> &str {
        self.config
            .projects
            .get(self.current)
            .map(|p| p.name.as_str())
            .unwrap_or("?")
    }

    /// Switches to another project (stops sessions, reloads store/repos/docs).
    pub fn load_project(&mut self, i: usize) {
        let Some(project) = self.config.projects.get(i).cloned() else {
            return;
        };
        self.stop_all_sessions();
        self.store = Store::new(&project.backlog);
        self.repos = project.repo_map();
        self.docs_dir = project.docs.clone();
        self.work_dir = project.work_dir.clone();
        self.conventions_path = project.conventions_path();
        self.conventions = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
        self.current = i;
        self.selected = 0;
        self.tab = Tab::Board;
        self.status_msg = format!("project: {}", project.name);
        let _ = self.refresh();
        self.rehydrate_sessions(); // persisted sessions of the new project
        self.start_watch(); // watch the new project's folder
    }

    // --- project picker ----------------------------------------------------

    pub fn open_picker(&mut self) {
        self.project_picker = true;
        self.picker_selected = self.current;
    }
    pub fn close_picker(&mut self) {
        self.project_picker = false;
    }
    pub fn picker_next(&mut self) {
        if !self.config.projects.is_empty() {
            self.picker_selected = (self.picker_selected + 1).min(self.config.projects.len() - 1);
        }
    }
    pub fn picker_prev(&mut self) {
        self.picker_selected = self.picker_selected.saturating_sub(1);
    }
    pub fn confirm_picker(&mut self) {
        let i = self.picker_selected;
        self.project_picker = false;
        if i != self.current {
            self.load_project(i);
        }
    }

    /// Reloads tasks, overlaps and the docs list from disk.
    pub fn refresh(&mut self) -> Result<()> {
        self.tasks = sort_for_board(self.store.list(None)?);
        if !self.tasks.is_empty() && self.selected >= self.tasks.len() {
            self.selected = self.tasks.len() - 1;
        }
        self.overlaps = Conflict::new(&self.store)
            .detect_overlap()
            .unwrap_or_default();
        // persisted parallelism analysis (best-effort; absent = no badges).
        self.parallel = std::fs::read_to_string(self.parallel_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        self.docs = list_docs(&self.docs_dir);
        if !self.docs.is_empty() && self.docs_selected >= self.docs.len() {
            self.docs_selected = self.docs.len() - 1;
        }
        Ok(())
    }

    // --- Board: task cards + panel focus -----------------------------------

    /// Middle-column cards for the selected task: one entry per live/history session
    /// of it + the verdict (if there's a `.review.md`). Session order follows
    /// `sessions` (already sorted by activity in `sort_sessions`).
    pub fn task_cards(&self) -> Vec<BoardCard> {
        // · project row: cards = setup sessions (no task).
        if self.project_selected {
            return self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, e)| e.kind == SessionKind::Setup)
                .map(|(i, _)| BoardCard::Session(i))
                .collect();
        }
        let Some(id) = self.selected_task().map(|t| t.id.clone()) else {
            return Vec::new();
        };
        let mut cards: Vec<BoardCard> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, e)| e.task.as_deref() == Some(id.as_str()))
            .map(|(i, _)| BoardCard::Session(i))
            .collect();
        if self.load_review(&id).is_some() {
            cards.push(BoardCard::Verdict);
        }
        cards
    }

    /// Card under the middle-column cursor.
    pub fn selected_card(&self) -> Option<BoardCard> {
        let cards = self.task_cards();
        cards
            .get(self.card_selected.min(cards.len().saturating_sub(1)))
            .copied()
    }

    /// `true` if the selected card is a live session (enables focusing the Chat).
    pub fn selected_card_is_live(&self) -> bool {
        matches!(self.selected_card(), Some(BoardCard::Session(i)) if self.sessions.get(i).is_some_and(|e| e.is_live()))
    }

    pub fn card_next(&mut self) {
        let n = self.task_cards().len();
        if n > 0 {
            self.card_selected = (self.card_selected + 1).min(n - 1);
        }
    }

    pub fn card_prev(&mut self) {
        self.card_selected = self.card_selected.saturating_sub(1);
    }

    /// Moves focus among the Board panels (Tasks → Cards → Chat). The Chat is only
    /// reachable when the selected card is a live session.
    pub fn focus_right(&mut self) {
        self.board_focus = match self.board_focus {
            BoardFocus::Tasks if !self.task_cards().is_empty() => BoardFocus::Cards,
            BoardFocus::Cards if self.selected_card_is_live() => BoardFocus::Chat,
            other => other,
        };
    }

    pub fn focus_left(&mut self) {
        self.board_focus = match self.board_focus {
            BoardFocus::Chat => BoardFocus::Cards,
            BoardFocus::Cards => BoardFocus::Tasks,
            BoardFocus::Tasks => BoardFocus::Tasks,
        };
    }

    // --- parallelism (Board badges) ----------------------------------------

    /// File of the persisted parallelism analysis.
    fn parallel_file(&self) -> PathBuf {
        self.work_dir.join("parallel.json")
    }

    /// "Active" tasks (parallelism reference): with a live play session OR in
    /// status `wip`.
    pub fn active_task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|e| e.is_live() && e.kind == SessionKind::Play)
            .filter_map(|e| e.task.clone())
            .collect();
        for t in &self.tasks {
            if t.status == Status::Wip && !ids.contains(&t.id) {
                ids.push(t.id.clone());
            }
        }
        ids
    }

    /// If the task collides with any ACTIVE task, returns `(other, repo, reason)`.
    pub fn parallel_conflict_with_active(&self, id: &str) -> Option<(String, String, String)> {
        let report = self.parallel.as_ref()?;
        for other in self.active_task_ids() {
            if other == id {
                continue;
            }
            if let Some(c) = report.conflict_between(id, &other) {
                return Some((other, c.repo.clone(), c.reason.clone()));
            }
        }
        None
    }

    /// `true` if there's analysis, another active task, and this one collides with
    /// none — i.e. it can be started in parallel safely.
    pub fn is_parallel_safe(&self, id: &str) -> bool {
        if self.parallel.is_none() {
            return false;
        }
        let has_other_active = self.active_task_ids().iter().any(|a| a != id);
        has_other_active && self.parallel_conflict_with_active(id).is_none()
    }

    // --- docs (Docs tab) ---------------------------------------------------

    pub fn docs_next(&mut self) {
        if !self.docs.is_empty() {
            self.docs_selected = (self.docs_selected + 1).min(self.docs.len() - 1);
        }
        self.doc_scroll = 0;
    }
    pub fn docs_prev(&mut self) {
        self.docs_selected = self.docs_selected.saturating_sub(1);
        self.doc_scroll = 0;
    }
    pub fn open_doc(&mut self) {
        if self.docs.get(self.docs_selected).is_none() {
            return;
        }
        // content is read fresh every frame in render (reflects external edits).
        self.doc_open = true;
        self.doc_scroll = 0;
    }
    pub fn close_doc(&mut self) {
        self.doc_open = false;
    }
    pub fn doc_scroll_down(&mut self) {
        self.doc_scroll = self.doc_scroll.saturating_add(1);
    }
    pub fn doc_scroll_up(&mut self) {
        self.doc_scroll = self.doc_scroll.saturating_sub(1);
    }

    pub fn selected_task(&self) -> Option<&Task> {
        if self.project_selected {
            return None;
        }
        self.tasks.get(self.selected)
    }

    /// Id of the TARGET task for actions (play/review/handoff/finish): the one selected in the Board.
    pub fn target_task_id(&self) -> Option<String> {
        self.selected_task().map(|t| t.id.clone())
    }

    pub fn select_next(&mut self) {
        if self.project_selected {
            // from · project (top) to the first task
            self.project_selected = false;
            self.selected = 0;
        } else if !self.tasks.is_empty() {
            self.selected = (self.selected + 1).min(self.tasks.len() - 1);
        }
        self.on_task_change();
    }

    pub fn select_prev(&mut self) {
        if self.project_selected {
            return; // already at the top
        }
        if self.selected == 0 {
            self.project_selected = true; // move up to · project
        } else {
            self.selected -= 1;
        }
        self.on_task_change();
    }

    pub fn select_first(&mut self) {
        self.project_selected = true; // top = · project
        self.selected = 0;
        self.on_task_change();
    }

    pub fn select_last(&mut self) {
        self.project_selected = false;
        self.selected = self.tasks.len().saturating_sub(1);
        self.on_task_change();
    }

    /// When the Board row changes, the cards cursor returns to the start.
    fn on_task_change(&mut self) {
        self.card_selected = 0;
    }

    /// Task's review report (if there's a `.review.md`). No session needed.
    pub fn load_review(&self, id: &str) -> Option<ReviewReport> {
        let path = self.store.review_path(id);
        self.store
            .read_doc::<ReviewReport>(&path)
            .ok()
            .map(|(r, _)| r)
    }

    // --- project setup ----------------------------------------------------

    /// External project folder (`~/jaum/<proj>`), derived from conventions_path.
    fn home(&self) -> PathBuf {
        self.conventions_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    /// What's missing from the mandatory setup (tasks without repo, template
    /// conventions, missing mapping).
    pub fn setup_needs(&self) -> SetupNeeds {
        SetupNeeds {
            unlinked: self
                .tasks
                .iter()
                .filter(|t| t.prs.is_empty())
                .map(|t| t.id.clone())
                .collect(),
            leaky_branches: self
                .tasks
                .iter()
                .filter(|t| t.prs.iter().any(|pr| branch_leaks_id(&pr.branch)))
                .map(|t| t.id.clone())
                .collect(),
            conventions_template: is_template(&self.conventions),
            mapping_missing: !self.home().join("setup.md").exists(),
        }
    }

    pub fn setup_needed(&self) -> bool {
        self.setup_needs().any()
    }

    /// Opens the interactive setup chat (claude may write in jaum's area).
    pub fn setup_start(&mut self) {
        let result = Setup::new(
            &self.store,
            &self.executor,
            self.home(),
            self.repos.clone(),
            self.conventions.clone(),
        )
        .start();
        match result {
            Ok((session, sid)) => {
                let cwd = self.home();
                self.open_session(SessionKind::Setup, None, session, Vec::new(), sid, cwd);
                self.status_msg = "setup: chat opened".into();
            }
            Err(e) => self.status_msg = format!("setup failed: {e}"),
        }
    }

    // --- actions (side effects) -------------------------------------------

    pub fn play_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            self.status_msg = "no task selected".into();
            return;
        };
        // already a live play session for this task? focus it instead of opening a
        // duplicate (two claudes in the same worktree would fight over the working dir).
        if let Some(idx) = self.sessions.iter().position(|e| {
            e.is_live() && e.kind == SessionKind::Play && e.task.as_deref() == Some(id.as_str())
        }) {
            self.focus_session(idx);
            self.status_msg = format!("play for {id} is already open");
            return;
        }
        let result = Play::new(
            &self.store,
            &self.git,
            self.repos.clone(),
            self.conventions.clone(),
        )
        .launch(&id)
        .and_then(|launch| self.open_play_session(launch));
        match result {
            Ok(()) => self.status_msg = format!("play started on {id}"),
            Err(e) => self.status_msg = format!("play failed: {e}"),
        }
        let _ = self.refresh();
    }

    // --- sidecar sessions (play) --------------------------------------------

    /// Spawns the sidecar if absent or dead (lazy supervision).
    fn ensure_sidecar(&mut self) -> Result<&SidecarClient> {
        let dead = match &mut self.sidecar {
            Some(client) => !client.is_alive(),
            None => true,
        };
        if dead {
            self.sidecar = Some(SidecarClient::spawn(&self.node_bin, &self.sidecar_bundle)?);
        }
        Ok(self.sidecar.as_ref().expect("just ensured"))
    }

    /// Sends the launch prompt as the session's first chat turn and registers
    /// the sidecar-backed entry.
    fn open_play_session(&mut self, launch: PlayLaunch) -> Result<()> {
        let log = SessionLog::new(&self.home(), &launch.session_id);
        let mut entry = SessionEntry::sidecar(
            SessionKind::Play,
            Some(launch.id.clone()),
            launch.worktrees.clone(),
            launch.session_id.clone(),
            launch.cwd.clone(),
            log,
        );
        let turn = ChatTurn {
            request_id: format!("{}#1", launch.session_id),
            session_id: launch.session_id.clone(),
            resume: None,
            cwd: Some(launch.cwd.clone()),
            model: Some(launch.guards.model.clone()),
            allowed_tools: Vec::new(),
            disallowed_tools: launch.guards.disallowed_tools.clone(),
            system_prompt_append: Some(launch.guards.system_prompt_append.clone()),
            guard_patterns: guard_patterns(&launch.guards),
            content: vec![ContentBlock::Text {
                text: launch.prompt.clone(),
            }],
        };
        let rx = self.ensure_sidecar()?.chat(turn)?;
        if let Some(chat) = &mut entry.chat {
            chat.rx = Some(rx);
            chat.request_id = Some(format!("{}#1", launch.session_id));
            chat.turn_seq = 1;
        }
        let key = entry.claude_session_id.clone();
        self.sessions.push(entry);
        self.sort_sessions();
        let idx = self
            .sessions
            .iter()
            .position(|e| e.claude_session_id == key)
            .unwrap_or(0);
        self.focus_session(idx);
        self.persist_sessions();
        Ok(())
    }

    /// Sends a follow-up user turn to a sidecar session, resuming the claude
    /// conversation. Queues the text when a turn is already in flight.
    fn send_chat_turn(&mut self, idx: usize, text: String) -> Result<()> {
        let (task_id, session_id, cwd, seq) = {
            let e = self
                .sessions
                .get_mut(idx)
                .context("session index out of range")?;
            let task_id = e.task.clone().context("session without a task")?;
            if e.turn_active() {
                let chat = e.chat.as_mut().context("not a sidecar session")?;
                chat.queued.push_back(text);
                return Ok(());
            }
            let chat = e.chat.as_mut().context("not a sidecar session")?;
            chat.turn_seq += 1;
            (
                task_id,
                e.claude_session_id.clone(),
                e.cwd.clone(),
                chat.turn_seq,
            )
        };
        let spec: GuardSpec = Play::new(
            &self.store,
            &self.git,
            self.repos.clone(),
            self.conventions.clone(),
        )
        .resume_spec(&task_id)?;
        let request_id = format!("{session_id}#{seq}");
        let turn = ChatTurn {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            resume: Some(session_id),
            cwd: Some(cwd),
            model: Some(spec.model.clone()),
            allowed_tools: Vec::new(),
            disallowed_tools: spec.disallowed_tools.clone(),
            system_prompt_append: Some(spec.system_prompt_append.clone()),
            guard_patterns: guard_patterns(&spec),
            content: vec![ContentBlock::Text { text }],
        };
        let rx = self.ensure_sidecar()?.chat(turn)?;
        if let Some(chat) = self.sessions[idx].chat.as_mut() {
            chat.rx = Some(rx);
            chat.request_id = Some(request_id);
        }
        self.sessions[idx].blocked = false;
        Ok(())
    }

    /// Pumps the sidecar events of every session: accumulates the log, feeds
    /// the text view, updates the claude session id and routes permissions.
    pub fn drain_sidecar(&mut self) {
        let route_permissions = self.route_permissions;
        let mut to_answer: Vec<String> = Vec::new();
        let mut to_track: Vec<(String, String)> = Vec::new();
        let mut to_send: Vec<(usize, String)> = Vec::new();
        let mut to_unregister: Vec<String> = Vec::new();
        let mut changed = false;

        for idx in 0..self.sessions.len() {
            let e = &mut self.sessions[idx];
            // take the receiver so the whole entry stays mutable during the
            // drain; it goes back unless the turn finished.
            let Some(rx) = e.chat.as_mut().and_then(|c| c.rx.take()) else {
                continue;
            };
            let mut turn_done = false;
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        match &event {
                            SidecarEvent::Session {
                                claude_session_id, ..
                            } if *claude_session_id != e.claude_session_id => {
                                e.claude_session_id = claude_session_id.clone();
                                changed = true;
                            }
                            SidecarEvent::PermissionRequest { permission_id, .. } => {
                                if route_permissions {
                                    to_track
                                        .push((e.claude_session_id.clone(), permission_id.clone()));
                                } else {
                                    to_answer.push(permission_id.clone());
                                }
                            }
                            SidecarEvent::Done { .. } | SidecarEvent::Error { .. } => {
                                turn_done = true;
                            }
                            _ => {}
                        }
                        if let Some(se) = event.to_session_event() {
                            if let Some(chat) = &e.chat {
                                let _ = chat.log.append(&se);
                            }
                            e.render_event(&se);
                        }
                        e.last_activity = SystemTime::now();
                        if turn_done {
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        turn_done = true;
                        break;
                    }
                }
            }
            let chat = e.chat.as_mut().expect("drained a chat session");
            if turn_done {
                if let Some(request_id) = chat.request_id.take() {
                    to_unregister.push(request_id);
                }
                if let Some(queued) = chat.queued.pop_front() {
                    to_send.push((idx, queued));
                }
                changed = true;
            } else {
                chat.rx = Some(rx);
            }
        }

        if let Some(client) = &self.sidecar {
            for request_id in to_unregister {
                client.unregister(&request_id);
            }
            for permission_id in to_answer {
                let _ = client.respond_permission(&permission_id, PermissionDecision::Allow);
            }
        }
        for (session_id, permission_id) in to_track {
            self.permissions.track(&session_id, &permission_id);
        }
        for (idx, text) in to_send {
            if let Err(e) = self.send_chat_turn(idx, text) {
                self.status_msg = format!("queued turn failed: {e}");
            }
        }
        if changed {
            self.sort_sessions();
            self.persist_sessions();
        }
    }

    /// Health check (~30s): pings the sidecar and kills it when the previous
    /// ping went unanswered (hung process); the next use respawns it.
    pub fn tick_sidecar_health(&mut self) {
        let Some(client) = &self.sidecar else {
            return;
        };
        if client.pong_seen() {
            self.sidecar_pinged = None;
        }
        if self.last_sidecar_ping.elapsed() < Duration::from_secs(30) {
            return;
        }
        self.last_sidecar_ping = Instant::now();
        if self.sidecar_pinged.is_some() {
            if let Some(mut client) = self.sidecar.take() {
                client.kill();
            }
            self.sidecar_pinged = None;
            self.status_msg = "sidecar unresponsive; restarting".into();
            return;
        }
        if self.sidecar.as_ref().is_some_and(|c| c.ping().is_ok()) {
            self.sidecar_pinged = Some(Instant::now());
        }
    }

    /// Expires unanswered permission requests: denies by default and marks
    /// the session blocked so the board surfaces it.
    pub fn tick_permissions(&mut self) {
        let expired = self.permissions.expired(Instant::now());
        if expired.is_empty() {
            return;
        }
        for pending in expired {
            if let Some(client) = &self.sidecar {
                let _ = client.respond_permission(
                    &pending.permission_id,
                    PermissionDecision::Deny {
                        message: Some("no decision within the deadline; denied by default".into()),
                    },
                );
            }
            if let Some(e) = self
                .sessions
                .iter_mut()
                .find(|e| e.claude_session_id == pending.session_id)
            {
                e.blocked = true;
                let event = ChatEvent::Error {
                    category: "permission_timeout".into(),
                    message: format!(
                        "permission {} denied by default after the deadline",
                        pending.permission_id
                    ),
                };
                if let Some(chat) = &e.chat {
                    let _ = chat.log.append(&event);
                }
                e.render_event(&event);
                self.status_msg = format!("session blocked: {}", e.name());
            }
        }
        self.persist_sessions();
    }

    pub fn review_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            return;
        };
        self.review_report = self.load_review(&id);
        let review = Review::new(
            &self.store,
            &self.git,
            &self.gh,
            &self.executor,
            &self.docs_dir,
            self.repos.clone(),
            self.conventions.clone(),
        );
        let cwd = review.review_cwd(&id);
        match review.start(&id) {
            Ok((session, sid)) => {
                self.open_session(
                    SessionKind::Review,
                    Some(id.clone()),
                    session,
                    Vec::new(),
                    sid,
                    cwd,
                );
                self.status_msg = format!("read-only review of {id}");
            }
            Err(e) => self.status_msg = format!("review failed: {e}"),
        }
    }

    /// Handoff: injects the review findings into the task's play session (opening
    /// one if none), so claude fixes the pending items.
    pub fn handoff_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            self.status_msg = "no task selected".into();
            return;
        };
        let Some(report) = self.load_review(&id) else {
            self.status_msg = "run review (R) before handoff".into();
            return;
        };
        if report.is_clean() {
            self.status_msg = "clean review — nothing to fix".into();
            return;
        }

        // find (or open) a live play session for the task
        let find = |s: &[SessionEntry]| {
            s.iter().position(|e| {
                e.is_live() && e.kind == SessionKind::Play && e.task.as_deref() == Some(id.as_str())
            })
        };
        let mut idx = find(&self.sessions);
        if idx.is_none() {
            self.play_selected();
            idx = find(&self.sessions);
        }
        let Some(idx) = idx else {
            return; // play failed (status already set)
        };

        let msg = jaum_flows::review::handoff_message(&report);
        match self.send_chat_turn(idx, msg) {
            Ok(()) => {
                self.focus_session(idx);
                self.status_msg = format!("{id} findings sent to play");
            }
            Err(e) => self.status_msg = format!("handoff failed: {e}"),
        }
    }

    pub fn finish_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            return;
        };
        match Finish::new(&self.store, &self.gh, self.repos.clone()).run(&id) {
            Ok(state) => {
                self.status_msg = format!("finish {id}: merge {state:?} (merge is your call)")
            }
            Err(e) => self.status_msg = format!("finish failed: {e}"),
        }
        let _ = self.refresh();
    }

    // --- async jobs (ingest/capture/init with live logs) ------------------

    /// Is a job still running?
    pub fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.finished)
    }

    /// Task whose review capture is running right now, if any. Drives the
    /// in-flight indicators (statusline label + list glyph) so the background
    /// auto-review isn't invisible once the "review started" toast fades.
    pub fn reviewing_task_id(&self) -> Option<&str> {
        self.job
            .as_ref()
            .filter(|j| !j.finished && j.kind == JobKind::Review)
            .and_then(|j| j.task.as_deref())
    }

    /// Path of the current project's `.backlog/` (to build a Store in the thread).
    fn backlog_path(&self) -> PathBuf {
        self.config
            .projects
            .get(self.current)
            .map(|p| p.backlog.clone())
            .unwrap_or_default()
    }

    /// Starts ingest in background, with live logs in the overlay.
    pub fn start_ingest_job(&mut self) {
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let add_dirs: Vec<PathBuf> = self.repos.values().cloned().collect();
        let claude_bin = self.claude_bin.clone();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Ingest,
            title: "ingest".into(),
            task: None,
            logs: vec!["scanning docs and repos with claude…".into()],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::with_bin(claude_bin);
            let ingest = Ingest::new(&store, &executor, docs_dir, add_dirs);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match ingest.run_logged(&mut on_line) {
                Ok(o) => Ok(format!(
                    "ingest: {} stub(s), {} doc(s)",
                    o.created.len(),
                    o.docs_imported
                )),
                Err(e) => Err(format!("ingest failed: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Runs the structured review capture of a task in background: a read-only
    /// `claude -p` that writes the `.review.md` (findings + verdicts). Dispatched
    /// by the CI watch when every PR check turns green — not by a keybinding —
    /// so it does NOT steal the screen with the job overlay.
    ///
    /// `markers` are the `(repo, head_sha)` to stamp as reviewed, applied ONLY
    /// after the capture succeeds: a failed run (claude error, rate limit) leaves
    /// the SHA unmarked so the next poll retries instead of skipping it forever.
    pub fn start_review_job_for(&mut self, id: &str, markers: Vec<(Repo, String)>) {
        let id = id.to_string();
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let repos = self.repos.clone();
        let conventions = self.conventions.clone();
        let claude_bin = self.claude_bin.clone();
        let gh_bin = self.gh_bin.clone();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Review,
            title: format!("review {id}"),
            task: Some(id.clone()),
            logs: vec![format!("reviewing {id} against docs and constraints…")],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.status_msg = format!("CI green on {id} — review started");
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let git = Git::new();
            let gh = Gh::with_bin(gh_bin);
            let executor = ClaudeExecutor::with_bin(claude_bin);
            let review = Review::new(&store, &git, &gh, &executor, docs_dir, repos, conventions);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match review.capture_logged(&id, &mut on_line) {
                Ok(r) => {
                    // stamp the reviewed SHAs only now that the report exists.
                    let _ = store.mark_reviewed(&id, &markers);
                    Ok(format!(
                        "review {id}: {} finding(s), {}",
                        r.findings.len(),
                        if r.is_clean() { "CLEAN" } else { "DIRTY" }
                    ))
                }
                Err(e) => Err(format!("review failed: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Runs the parallelism analysis (which tasks collide) in background, writes
    /// `parallel.json` and refreshes the Board badges when done.
    pub fn start_parallel_job(&mut self) {
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let root = self.docs_dir.clone();
        let repos = self.repos.clone();
        let out_file = self.parallel_file();
        let claude_bin = self.claude_bin.clone();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Parallel,
            title: "parallelism analysis".into(),
            task: None,
            logs: vec!["analyzing which tasks collide…".into()],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::with_bin(claude_bin);
            let parallel = Parallel::new(&store, &executor, root, repos);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match parallel.analyze_logged(&mut on_line) {
                Ok(r) => {
                    if let Some(parent) = out_file.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(json) = serde_json::to_string_pretty(&r) {
                        let _ = std::fs::write(&out_file, json);
                    }
                    Ok(format!("parallelism: {} conflict(s)", r.conflicts.len()))
                }
                Err(e) => Err(format!("parallelism analysis failed: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Starts the investigated capture (user hint) in background.
    pub fn start_capture_job(&mut self, hint: &str) {
        let hint = hint.trim().to_string();
        if hint.is_empty() {
            self.status_msg = "capture cancelled (empty)".into();
            return;
        }
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let add_dirs: Vec<PathBuf> = self.repos.values().cloned().collect();
        let claude_bin = self.claude_bin.clone();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Capture,
            title: "capture (claude investigates)".into(),
            task: None,
            logs: vec![format!("investigating: {hint}")],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::with_bin(claude_bin);
            let ingest = Ingest::new(&store, &executor, docs_dir, add_dirs);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match ingest.capture_logged(&hint, &mut on_line) {
                Ok(o) => {
                    let ids: Vec<String> = o.created.iter().map(|t| t.id.clone()).collect();
                    Ok(format!("claude created: {}", ids.join(", ")))
                }
                Err(e) => Err(format!("capture failed: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Starts `init` of a new project in background (detects repos and registers).
    pub fn start_init_job(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            self.status_msg = "init cancelled (empty)".into();
            return;
        }
        if self.job_running() {
            return;
        }
        let root = expand_tilde(path);
        let init_project = self.init_project;
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Init,
            title: format!("init {path}"),
            task: None,
            logs: vec![format!("detecting repos in {}", root.display())],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let done = match init_project(&root, &[]) {
                Ok(p) => {
                    let _ = tx.send(JobMsg::Log(format!("project '{}' registered", p.name)));
                    if p.repos.is_empty() {
                        let _ = tx.send(JobMsg::Log("no repos detected".into()));
                    }
                    for r in &p.repos {
                        let _ = tx.send(JobMsg::Log(format!(
                            "repo {} -> {}",
                            r.slug,
                            r.path.display()
                        )));
                    }
                    Ok(p.name)
                }
                Err(e) => Err(format!("init failed: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Drains the current job's messages (called every frame by the event loop).
    pub fn poll_job(&mut self) {
        let mut outcome: Option<(JobKind, Result<String, String>)> = None;
        if let Some(job) = self.job.as_mut() {
            while let Ok(msg) = job.rx.try_recv() {
                match msg {
                    JobMsg::Log(l) => job.logs.push(l),
                    JobMsg::Done(r) => {
                        job.finished = true;
                        outcome = Some((job.kind, r));
                    }
                }
            }
        }
        let Some((kind, r)) = outcome else { return };
        // a successful ingest chains into the parallelism analysis.
        let auto_parallel = matches!(kind, JobKind::Ingest) && r.is_ok();
        match (kind, r) {
            (JobKind::Init, Ok(name)) => {
                if let Ok(cfg) = (self.load_config)() {
                    self.config = cfg;
                    if let Some(idx) = self.config.projects.iter().position(|p| p.name == name) {
                        self.load_project(idx);
                    }
                }
                if self.setup_needed() {
                    self.status_msg = format!("project '{name}' registered — setup pending (S)");
                } else {
                    self.status_msg = format!("project '{name}' registered");
                }
                if let Some(j) = self.job.as_mut() {
                    j.logs.push(format!("— project '{name}' ready"));
                }
            }
            (_, Ok(msg)) | (_, Err(msg)) => {
                self.status_msg = msg.clone();
                if let Some(j) = self.job.as_mut() {
                    j.logs.push(format!("— {msg}"));
                }
                let _ = self.refresh();
            }
        }
        if auto_parallel {
            self.start_parallel_job();
        }
    }

    /// Closes the logs overlay (the job, if still running, keeps going in background).
    pub fn dismiss_job(&mut self) {
        self.job_overlay = false;
        if self.job.as_ref().is_some_and(|j| j.finished) {
            self.job = None;
        }
    }

    /// Scrolls up the job logs (turns off follow to lock reading in place).
    pub fn job_scroll_up(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = j.scroll.saturating_sub(1);
        }
    }

    /// Scrolls down the job logs. On reaching the end, re-enables follow (live).
    pub fn job_scroll_down(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = j.scroll.saturating_add(1);
        }
    }

    /// Jumps to the top of the logs.
    pub fn job_scroll_top(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = 0;
        }
    }

    /// Resumes following the live tail.
    pub fn job_follow(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = true;
        }
    }

    /// Records extra scope as deferred and creates a new backlog.
    pub fn defer(&mut self, text: &str) {
        let Some(id) = self.target_task_id() else {
            return;
        };
        if text.trim().is_empty() {
            self.status_msg = "defer cancelled (empty text)".into();
            return;
        }
        match self.store.add_deferred(&id, text) {
            Ok(new) => self.status_msg = format!("deferred from {id} -> {}", new.id),
            Err(e) => self.status_msg = format!("defer failed: {e}"),
        }
        let _ = self.refresh();
    }

    /// Rereads on-disk state to pick up external edits (e.g. what the setup session
    /// wrote). Reacts IMMEDIATELY to file-watcher events; with a time fallback
    /// (~2s) in case the watcher isn't available. Preserves the selection.
    pub fn tick_reload(&mut self) {
        let fs_event = self
            .watch_rx
            .as_ref()
            .map(|rx| rx.try_iter().count() > 0)
            .unwrap_or(false);
        if fs_event || self.last_reload.elapsed() >= Duration::from_millis(2000) {
            self.last_reload = Instant::now();
            self.reload_conventions();
            let _ = self.refresh();
        }
    }

    /// `(id, repo, branch)` of branches still without a PR (`pr == 0`) for tasks
    /// with a live play session. The target of the background PR sync.
    pub fn pr_sync_targets(&self) -> Vec<(String, String, String)> {
        let mut targets = Vec::new();
        for e in &self.sessions {
            if !(e.is_live() && e.kind == SessionKind::Play) {
                continue;
            }
            let Some(id) = e.task.as_deref() else {
                continue;
            };
            if let Some(t) = self.tasks.iter().find(|t| t.id == id) {
                for link in t.prs.iter().filter(|l| l.pr == 0) {
                    targets.push((id.to_string(), link.repo.clone(), link.branch.clone()));
                }
            }
        }
        targets
    }

    /// Discovers and writes, in background, the PR number of branches for tasks with
    /// a live play session (throttle ~20s). The agent opens the PR in the worktree;
    /// here we only query gh and persist with `set_pr` — the watcher updates the UI.
    pub fn tick_pr_sync(&mut self) {
        if self.pr_sync_running.load(Ordering::Relaxed) {
            return;
        }
        if self.last_pr_sync.elapsed() < Duration::from_secs(20) {
            return;
        }
        self.last_pr_sync = Instant::now();

        let targets = self.pr_sync_targets();
        if targets.is_empty() {
            return;
        }

        let backlog = self.backlog_path();
        let repos = self.repos.clone();
        let gh_bin = self.gh_bin.clone();
        let flag = self.pr_sync_running.clone();
        flag.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let gh = Gh::with_bin(gh_bin);
            for (id, repo, branch) in targets {
                if let Some(dir) = repos.get(&repo)
                    && let Ok(pr) = gh.pr_number(dir, &branch)
                    && pr != 0
                {
                    let _ = store.set_pr(&id, &repo, pr);
                }
            }
            flag.store(false, Ordering::Relaxed);
        });
    }

    // --- CI watch (auto review on green checks) ----------------------------

    /// `(id, [(repo, pr)])` of the tasks whose CI is worth polling: every link
    /// already has a PR (`pr != 0`) and the task is not merged/spike. Openness
    /// is confirmed by the poll itself (it comes back in the same gh call).
    pub fn ci_watch_targets(&self) -> Vec<(String, Vec<(Repo, u64)>)> {
        self.tasks
            .iter()
            .filter(|t| {
                !t.is_spike()
                    && t.status != Status::Merged
                    && !t.prs.is_empty()
                    && t.prs.iter().all(|l| l.pr != 0)
            })
            .map(|t| {
                let prs = t.prs.iter().map(|l| (l.repo.clone(), l.pr)).collect();
                (t.id.clone(), prs)
            })
            .collect()
    }

    /// Polls, in background, the CI of the eligible tasks' PRs (throttled by
    /// `ci_poll_interval`) and dispatches the automatic review for whatever
    /// turned green. A gh failure downgrades that PR to `Unknown`, which never
    /// triggers. Called every frame by the daemon loop.
    pub fn tick_ci_watch(&mut self) {
        self.drain_ci_results();
        if self.ci_poll_running.load(Ordering::Relaxed) {
            return;
        }
        if self.last_ci_poll.elapsed() < self.ci_poll_interval {
            return;
        }
        self.last_ci_poll = Instant::now();

        let targets = self.ci_watch_targets();
        if targets.is_empty() {
            return;
        }

        let repos = self.repos.clone();
        let gh_bin = self.gh_bin.clone();
        let tx = self.ci_tx.clone();
        let flag = self.ci_poll_running.clone();
        flag.store(true, Ordering::Relaxed);
        std::thread::spawn(move || {
            let gh = Gh::with_bin(gh_bin);
            for (id, prs) in targets {
                let observed: Vec<(Repo, PrCi)> = prs
                    .into_iter()
                    .map(|(repo, pr)| {
                        let ci = repos
                            .get(&repo)
                            .map(|dir| gh.pr_ci(dir, pr))
                            .and_then(Result::ok)
                            .unwrap_or(PrCi {
                                state: MergeState::Unknown,
                                checks: CiStatus::Unknown,
                                head_sha: String::new(),
                            });
                        (repo, ci)
                    })
                    .collect();
                if tx.send((id, observed)).is_err() {
                    break;
                }
            }
            flag.store(false, Ordering::Relaxed);
        });
    }

    /// Applies the CI observations queued by the polling thread: records the
    /// observation (for the board's pending-review indicator), re-reads the task
    /// from disk (the source of truth may have moved), asks the trigger and
    /// starts the review job with the SHAs to stamp on success. With a job
    /// already running the observation is dropped — the SHA stays unmarked, so
    /// the next poll re-arms the trigger.
    fn drain_ci_results(&mut self) {
        let pending: Vec<(String, Vec<(Repo, PrCi)>)> = self.ci_rx.try_iter().collect();
        for (id, observed) in pending {
            self.ci_obs.insert(id.clone(), observed.clone());
            let Ok(task) = self.store.get(&id) else {
                continue;
            };
            let Some(markers) = task.review_trigger(&observed) else {
                continue;
            };
            if self.job_running() {
                continue;
            }
            self.start_review_job_for(&id, markers);
        }
    }

    /// Review status of a task, for the board indicators. Derived from the last
    /// CI observation vs the persisted `reviewed_sha`: a re-review is due when an
    /// open PR's head moved past what was reviewed. `Running` wins (a capture is
    /// in flight); otherwise the aggregate CI of the unreviewed heads decides.
    pub fn review_progress(&self, task: &Task) -> Option<ReviewProgress> {
        if self.reviewing_task_id() == Some(task.id.as_str()) {
            return Some(ReviewProgress::Running);
        }
        let obs = self.ci_obs.get(&task.id)?;
        let mut pending_review = false;
        let mut agg = CiStatus::Passing;
        for link in &task.prs {
            let Some((_, ci)) = obs.iter().find(|(repo, _)| *repo == link.repo) else {
                continue;
            };
            if ci.state != MergeState::Open || ci.head_sha.is_empty() {
                continue;
            }
            // this PR carries a commit that has not been reviewed yet.
            if link.reviewed_sha.as_deref() != Some(ci.head_sha.as_str()) {
                pending_review = true;
                agg = match (agg, ci.checks) {
                    (_, CiStatus::Failing) | (CiStatus::Failing, _) => CiStatus::Failing,
                    (_, CiStatus::Pending) | (CiStatus::Pending, _) => CiStatus::Pending,
                    (_, CiStatus::Unknown) | (CiStatus::Unknown, _) => CiStatus::Unknown,
                    _ => CiStatus::Passing,
                };
            }
        }
        if !pending_review {
            return None;
        }
        match agg {
            // all green: the trigger fires on the next tick, so this is transient.
            CiStatus::Passing => None,
            CiStatus::Pending => Some(ReviewProgress::AwaitingCi),
            CiStatus::Failing => Some(ReviewProgress::CiFailed),
            CiStatus::Unknown | CiStatus::NoChecks => None,
        }
    }

    // --- toast (temporary snackbar) ---------------------------------------

    /// Detects a change in `status_msg` and (re)starts the toast timer. Called every
    /// frame by the event loop.
    pub fn tick_toast(&mut self) {
        if self.status_msg != self.toast_shown {
            self.toast_shown = self.status_msg.clone();
            self.toast_started = Some(Instant::now());
        }
    }

    /// (Re)shows the toast with the current message NOW, even if the text hasn't
    /// changed. Used after an action to give retry feedback (e.g. retrying a command
    /// that fails the same way shows the error again instead of silence).
    pub fn rearm_toast(&mut self) {
        self.toast_shown = self.status_msg.clone();
        self.toast_started = Some(Instant::now());
    }

    /// Toast text while visible (a few seconds), otherwise `None`.
    pub fn active_toast(&self) -> Option<&str> {
        self.toast_started
            .filter(|t| t.elapsed() < Duration::from_millis(3500))
            .map(|_| self.status_msg.as_str())
    }

    // --- quick capture -----------------------------------------------------

    /// Dispatches the captured text per kind.
    pub fn submit_input(&mut self, kind: InputKind, text: String) {
        match kind {
            InputKind::Defer => self.defer(&text),
            InputKind::Convention => self.add_convention(&text),
            InputKind::NewTask => self.new_task_quick(&text),
            InputKind::NewTaskClaude => self.start_capture_job(&text),
            InputKind::InitPath => self.start_init_job(&text),
        }
    }

    /// Applies a domain intent (the only mutation path shared by the daemon
    /// and the local TUI). Action intents re-show the toast (retry feedback):
    /// retrying a command that fails the same way shows the error again
    /// instead of going silent.
    pub fn apply_intent(&mut self, intent: Intent) {
        let rearm = matches!(
            intent,
            Intent::Play
                | Intent::ReviewChat
                | Intent::Handoff
                | Intent::Finish
                | Intent::Ingest
                | Intent::AnalyzeParallel
                | Intent::StartSetup
                | Intent::StartInput {
                    kind: InputKind::InitPath,
                    ..
                }
        );
        match intent {
            Intent::Quit => self.should_quit = true,
            Intent::NextTab => self.tab = self.tab.next(),
            Intent::SetTab { index } => self.tab = Tab::from_index(index),
            Intent::SelectNext => self.select_next(),
            Intent::SelectPrev => self.select_prev(),
            Intent::SelectFirst => self.select_first(),
            Intent::SelectLast => self.select_last(),
            Intent::FocusLeft => self.focus_left(),
            Intent::FocusRight => self.focus_right(),
            Intent::CardNext => self.card_next(),
            Intent::CardPrev => self.card_prev(),
            Intent::OpenDetail => self.open_detail(),
            Intent::CloseDetail => self.close_detail(),
            Intent::DetailScrollDown => self.detail_scroll_down(),
            Intent::DetailScrollUp => self.detail_scroll_up(),
            Intent::DocsNext => self.docs_next(),
            Intent::DocsPrev => self.docs_prev(),
            Intent::OpenDoc => self.open_doc(),
            Intent::CloseDoc => self.close_doc(),
            Intent::DocScrollDown => self.doc_scroll_down(),
            Intent::DocScrollUp => self.doc_scroll_up(),
            Intent::OpenPicker => self.open_picker(),
            Intent::ClosePicker => self.close_picker(),
            Intent::PickerNext => self.picker_next(),
            Intent::PickerPrev => self.picker_prev(),
            Intent::PickerConfirm => self.confirm_picker(),
            Intent::DismissJob => self.dismiss_job(),
            Intent::JobScrollUp => self.job_scroll_up(),
            Intent::JobScrollDown => self.job_scroll_down(),
            Intent::JobScrollTop => self.job_scroll_top(),
            Intent::JobFollow => self.job_follow(),
            Intent::ToggleZoom => self.chat_fullscreen = !self.chat_fullscreen,
            Intent::Play => self.play_selected(),
            Intent::ReviewChat => self.review_selected(),
            Intent::Handoff => self.handoff_selected(),
            Intent::Finish => self.finish_selected(),
            Intent::Ingest => self.start_ingest_job(),
            Intent::AnalyzeParallel => self.start_parallel_job(),
            Intent::StartSetup => self.setup_start(),
            Intent::EditConventions => self.request_edit_conventions(),
            Intent::StartInput { kind, prefill } => self.input = Some((kind, prefill)),
            Intent::InputChar { ch } => {
                if let Some((_, buf)) = self.input.as_mut() {
                    buf.push(ch);
                }
            }
            Intent::InputBackspace => {
                if let Some((_, buf)) = self.input.as_mut() {
                    buf.pop();
                }
            }
            Intent::InputCancel => self.input = None,
            Intent::InputSubmit => {
                if let Some((kind, text)) = self.input.take() {
                    self.submit_input(kind, text);
                }
            }
            Intent::FinishSession => self.finish_selected_session(),
            Intent::CloseSession => self.close_selected_session(),
        }
        if rearm {
            self.rearm_toast();
        }
    }

    /// Appends a convention to `conventions.md` (applies to future sessions).
    pub fn add_convention(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status_msg = "convention cancelled (empty)".into();
            return;
        }
        let mut content = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("- {text}\n"));
        match std::fs::write(&self.conventions_path, &content) {
            Ok(()) => {
                self.conventions = content;
                self.status_msg = "convention added".into();
            }
            Err(e) => self.status_msg = format!("failed to write convention: {e}"),
        }
    }

    /// Creates a quick task: the text becomes the objective (no LLM).
    pub fn new_task_quick(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status_msg = "task cancelled (empty)".into();
            return;
        }
        let body = format!("## Objective\n\n{text}\n\n## Acceptance criteria\n- [ ] \n");
        match self
            .store
            .create_backlog(jaum_core::TaskType::Impl, Vec::new(), Vec::new(), body)
        {
            Ok(t) => self.status_msg = format!("task created: {}", t.id),
            Err(e) => self.status_msg = format!("failed to create task: {e}"),
        }
        let _ = self.refresh();
    }

    // --- sessions (multi, parallel) ---------------------------------------

    /// Opens a new session and focuses its card in the Board (chat).
    pub(crate) fn open_session(
        &mut self,
        kind: SessionKind,
        task: Option<String>,
        session: Session,
        worktrees: Vec<(String, PathBuf)>,
        claude_session_id: String,
        cwd: PathBuf,
    ) {
        self.session_events.push(SessionEvent {
            session_id: claude_session_id.clone(),
            kind: SessionEventKind::Started {
                kind: crate::snapshot::wire_session_kind(kind),
                task: task.clone(),
            },
        });
        self.sessions.push(SessionEntry::spawn(
            kind,
            task,
            session,
            worktrees,
            claude_session_id.clone(),
            cwd,
        ));
        self.sort_sessions(); // the just-created one (activity = now) goes to the top
        let idx = self
            .sessions
            .iter()
            .position(|e| e.claude_session_id == claude_session_id)
            .unwrap_or(0);
        self.focus_session(idx);
        self.persist_sessions();
    }

    /// Focuses (in the Board) session `idx`: selects the owner task and puts the
    /// cursor on its card; enters the chat if the session is live.
    pub(crate) fn focus_session(&mut self, idx: usize) {
        self.tab = Tab::Board;
        match self.sessions.get(idx).and_then(|e| e.task.clone()) {
            // session of a task: select the owner task.
            Some(task_id) => {
                self.project_selected = false;
                if let Some(pos) = self.tasks.iter().position(|t| t.id == task_id) {
                    self.selected = pos;
                }
            }
            // setup session (no task): the · project row.
            None => self.project_selected = true,
        }
        self.card_selected = self
            .task_cards()
            .iter()
            .position(|c| *c == BoardCard::Session(idx))
            .unwrap_or(0);
        self.board_focus = if self.selected_card_is_live() {
            BoardFocus::Chat
        } else {
            BoardFocus::Cards
        };
    }

    /// Index, in `sessions`, of the session under the selected card (if it's a session).
    pub fn current_session_idx(&self) -> Option<usize> {
        match self.selected_card() {
            Some(BoardCard::Session(i)) => Some(i),
            _ => None,
        }
    }

    /// Path of the persisted sessions file (survives shutdown).
    fn sessions_file(&self) -> PathBuf {
        self.work_dir.join("sessions.json")
    }

    /// Writes the sessions snapshot to disk (best-effort, never crashes the TUI).
    pub(crate) fn persist_sessions(&self) {
        let records: Vec<SessionRecord> = self.sessions.iter().map(|e| e.to_record()).collect();
        if let Some(parent) = self.sessions_file().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&records) {
            let _ = std::fs::write(self.sessions_file(), json);
        }
    }

    /// Reads the persisted records (empty if the file is missing or invalid).
    fn load_session_records(&self) -> Vec<SessionRecord> {
        std::fs::read_to_string(self.sessions_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Removes a play session's worktrees (cleanup on close). The branch stays in
    /// the repo (the worktree is just the working copy).
    fn cleanup_worktrees(&self, task: &Option<String>, worktrees: &[(String, PathBuf)]) {
        let Some(id) = task else { return };
        for (repo, _) in worktrees {
            if let Ok(task) = self.store.get(id)
                && let Some(link) = task.prs.iter().find(|p| &p.repo == repo)
                && let Some(repo_path) = self.repos.get(repo)
            {
                let _ = self.git.worktree_remove(repo_path, &link.branch);
            }
        }
    }

    /// Aborts a sidecar session's in-flight turn (no-op for PTY sessions).
    fn abort_chat_turn(&mut self, idx: usize) {
        let request_id = self
            .sessions
            .get_mut(idx)
            .and_then(|e| e.chat.as_mut())
            .and_then(|chat| {
                chat.rx = None;
                chat.request_id.take()
            });
        if let (Some(request_id), Some(client)) = (request_id, &self.sidecar) {
            let _ = client.abort(&request_id);
            client.unregister(&request_id);
        }
    }

    /// Finishes the selected card's session: stops the process and cleans the
    /// worktrees, but KEEPS it in the list as done (✓), session history.
    pub fn finish_selected_session(&mut self) {
        let Some(idx) = self.current_session_idx() else {
            return;
        };
        self.abort_chat_turn(idx);
        let was_live = self.sessions[idx].is_live();
        if let Some(s) = &mut self.sessions[idx].session {
            let _ = s.kill();
        }
        self.sessions[idx].session = None;
        self.sessions[idx].chat = None;
        let task = self.sessions[idx].task.clone();
        let worktrees = self.sessions[idx].worktrees.clone();
        self.cleanup_worktrees(&task, &worktrees);
        self.sessions[idx].finished = true;
        // history entries were already reported as finished; no duplicate event.
        if was_live {
            self.session_events.push(SessionEvent {
                session_id: self.sessions[idx].claude_session_id.clone(),
                kind: SessionEventKind::Finished,
            });
        }
        self.status_msg = format!("session finished: {}", self.sessions[idx].name());
        self.persist_sessions();
    }

    /// Removes the selected card's session from the list (stops it if still running).
    pub fn close_selected_session(&mut self) {
        let Some(idx) = self.current_session_idx() else {
            return;
        };
        self.abort_chat_turn(idx);
        let mut e = self.sessions.remove(idx);
        let was_live = e.is_live();
        if let Some(s) = &mut e.session {
            let _ = s.kill();
        }
        // history entries were already reported as finished; no duplicate event.
        if was_live {
            self.session_events.push(SessionEvent {
                session_id: e.claude_session_id.clone(),
                kind: SessionEventKind::Finished,
            });
        }
        self.cleanup_worktrees(&e.task, &e.worktrees);
        // the cards cursor recomputes; if we were in chat, fall back to cards.
        self.card_selected = self.card_selected.saturating_sub(1);
        if self.board_focus == BoardFocus::Chat {
            self.board_focus = BoardFocus::Cards;
        }
        self.persist_sessions();
    }

    /// Stops ALL sessions (project switch / daemon shutdown). PRESERVES the
    /// worktrees and the on-disk record: live ones come back resumed on the next
    /// boot. Worktree removal only happens on explicit `finish`/`close`.
    pub fn stop_all_sessions(&mut self) {
        self.persist_sessions();
        for idx in 0..self.sessions.len() {
            self.abort_chat_turn(idx);
        }
        for e in &mut self.sessions {
            if let Some(s) = &mut e.session {
                let _ = s.kill();
            }
        }
        self.sessions.clear();
        self.card_selected = 0;
    }

    /// Applies each PTY's pending bytes to its vt100 parser, emitting session
    /// events (output/finished) for connected clients.
    pub fn drain_pty(&mut self) {
        for e in &mut self.sessions {
            let (bytes, finished_now) = e.drain();
            if !bytes.is_empty() {
                self.session_events.push(SessionEvent {
                    session_id: e.claude_session_id.clone(),
                    kind: SessionEventKind::Output { bytes },
                });
            }
            if finished_now {
                self.session_events.push(SessionEvent {
                    session_id: e.claude_session_id.clone(),
                    kind: SessionEventKind::Finished,
                });
            }
        }
        // `drain` updates `last_activity`; reorder to keep the most recent on top
        // (without losing the selected session).
        self.sort_sessions();
    }

    /// Takes the session events accumulated since the last call.
    pub fn take_session_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.session_events)
    }

    /// Sorts sessions by activity (most recent on top), repositioning the cards
    /// cursor on the SAME session (by uuid) so focus isn't lost.
    pub fn sort_sessions(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        let focused = self
            .current_session_idx()
            .map(|i| self.sessions[i].claude_session_id.clone());
        // most recent first; ties on activity break by creation (seq), so the
        // newest session ends up on top deterministically.
        self.sessions.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then(b.seq.cmp(&a.seq))
        });
        if let Some(uuid) = focused
            && let Some(new_idx) = self
                .sessions
                .iter()
                .position(|e| e.claude_session_id == uuid)
            && let Some(pos) = self
                .task_cards()
                .iter()
                .position(|c| *c == BoardCard::Session(new_idx))
        {
            self.card_selected = pos;
        }
    }
}

// Unit tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file; the coverage tooling drops any path containing
// a `tests/` segment, which discards `#[path]` includes from integration tests.
#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;
