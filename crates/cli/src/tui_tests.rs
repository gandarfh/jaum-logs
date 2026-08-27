//! Headless tests for the TUI render/event module: draws every screen state
//! into a ratatui TestBackend and asserts on meaningful content.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::app::{self, App, BoardFocus, InputKind, SessionKind, Tab};
use crate::config;
use crate::tui;
use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-tui-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn task_md(id: &str, status: &str) -> String {
    format!(
        "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: org/x\n    pr: 0\n    branch: feat/{id}\n---\n\n## Objective\nx\n"
    )
}

fn write_task(dir: &TmpDir, id: &str, content: &str) {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join(format!("{id}.md")), content).unwrap();
}

fn app_with(dir: &TmpDir, tasks: &[(&str, &str)]) -> App {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for (id, status) in tasks {
        write_task(dir, id, &task_md(id, status));
    }
    let project = config::Project {
        name: "test".into(),
        root: dir.0.clone(),
        backlog,
        docs: dir.0.join("docs"),
        work_dir: dir.0.join(".jaum"),
        repos: Vec::new(),
    };
    let cfg = config::Config {
        projects: vec![project],
    };
    let mut a = App::new(cfg, 0).unwrap();
    // safety net: any executor spawn in a test becomes `cat`, never `claude`.
    a.executor = ClaudeExecutor::with_bin("cat");
    a
}

fn cat_session() -> jaum_adapters::Session {
    ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap()
}

fn open_cat(a: &mut App, kind: SessionKind, task: Option<&str>, uuid: &str, cwd: &TmpDir) {
    a.open_session(
        kind,
        task.map(str::to_string),
        cat_session(),
        Vec::new(),
        uuid.into(),
        cwd.0.clone(),
    );
}

fn draw(a: &App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let snap = crate::snapshot::build_snapshot(a);
    let mut lists = tui::ListStates::default();
    term.draw(|f| tui::render(f, &snap, Some(a), &mut lists))
        .unwrap();
    term
}

/// Renders the way a socket client does: same snapshot, no local `App`.
fn draw_remote(a: &App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let snap = crate::snapshot::build_snapshot(a);
    let mut lists = tui::ListStates::default();
    term.draw(|f| tui::render(f, &snap, None, &mut lists))
        .unwrap();
    term
}

fn buf_string(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn has_bg(term: &Terminal<TestBackend>, color: Color) -> bool {
    term.backend()
        .buffer()
        .content
        .iter()
        .any(|c| c.bg == color)
}

const SEL_BG_COLOR: Color = Color::Rgb(54, 48, 92);

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn fake_job(
    logs: &[&str],
    finished: bool,
    follow: bool,
    scroll: u16,
) -> (app::Job, std::sync::mpsc::Sender<app::JobMsg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    (
        app::Job {
            kind: app::JobKind::Init,
            title: "init".into(),
            logs: logs.iter().map(|s| s.to_string()).collect(),
            rx,
            finished,
            scroll,
            follow,
        },
        tx,
    )
}

// --- render: board ----------------------------------------------------------

#[test]
fn board_renders_status_groups_badges_and_selection() {
    let dir = TmpDir::new("board");
    let a = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "review"),
            ("TASK-003", "ready"),
            ("TASK-004", "backlog"),
            ("TASK-005", "merged"),
        ],
    );
    let term = draw(&a, 120, 40);
    let s = buf_string(&term);
    assert!(s.contains("◆ jaum"), "logo in header");
    assert!(s.contains("1 Board") && s.contains("2 Docs"), "tab pills");
    assert!(s.contains(" test "), "project name on the right");
    assert!(s.contains("Board · 5"), "list title with count");
    for group in ["WIP", "REVIEW", "READY", "BACKLOG", "MERGED"] {
        assert!(s.contains(group), "status group {group}");
    }
    assert!(s.contains("· project"), "synthetic project row");
    assert!(s.contains("▶"), "wip badge");
    assert!(s.contains("✔"), "merged badge");
    assert!(s.contains("TASK-001"));
    assert!(has_bg(&term, SEL_BG_COLOR), "selected row highlight");
    // footer key hints
    assert!(s.contains("play") && s.contains("quit"));
    assert!(s.contains("[Board]"), "statusline tab");
}

/// Regression: `render_board_list` used to build a fresh `ListState` on every
/// draw, so ratatui always recomputed the visible window from offset 0. Going
/// down this coincidentally looks right (the window only ever grows to reveal
/// the selection), but going up it recomputes `offset = selected -
/// visible_height + 1` from scratch on every single step — no plateau — so
/// the very first step up from the bottom already scrolls the viewport,
/// instead of just moving the cursor within the still-fully-visible window.
/// With a `ListStates` persisted across frames (matching real usage: one
/// `render` call per key), the offset must stay put on that first step up.
#[test]
fn board_list_scroll_holds_viewport_on_first_step_up() {
    let dir = TmpDir::new("scroll");
    let ids: Vec<String> = (0..40).map(|i| format!("TASK-{i:03}")).collect();
    let tasks: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "backlog")).collect();
    let mut a = app_with(&dir, &tasks);

    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let mut lists = tui::ListStates::default();

    let mut down = Vec::new();
    for i in 0..a.tasks.len() {
        a.selected = i;
        let snap = crate::snapshot::build_snapshot(&a);
        term.draw(|f| tui::render(f, &snap, Some(&a), &mut lists))
            .unwrap();
        down.push(lists.board.offset());
    }
    // sanity: the list must have actually needed to scroll in this terminal size.
    assert!(
        *down.last().unwrap() > 0,
        "offsets {down:?} never scrolled — test setup doesn't exercise the bug"
    );
    for w in down.windows(2) {
        let delta = w[1].abs_diff(w[0]);
        assert!(delta <= 1, "offsets {down:?} jumped by {delta} going down");
    }

    // one step up from the last row: the cursor was at the bottom of an
    // already-scrolled window, so it still has room to move up inside that
    // same window — the offset must not change yet.
    a.selected = a.tasks.len() - 2;
    let snap = crate::snapshot::build_snapshot(&a);
    term.draw(|f| tui::render(f, &snap, Some(&a), &mut lists))
        .unwrap();
    let offset_after_one_step_up = lists.board.offset();
    assert_eq!(
        offset_after_one_step_up,
        *down.last().unwrap(),
        "the viewport scrolled on the very first step up instead of moving the cursor within it"
    );
}

#[test]
fn board_project_row_shows_setup_state() {
    let dir = TmpDir::new("projrow");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Setup, None, "u-setup", &dir);
    a.select_first(); // · project row
    assert!(a.setup_needed(), "tmp project has no setup.md");
    let s = buf_string(&draw(&a, 180, 40));
    assert!(
        s.contains("· project ●"),
        "live setup dot on the project row"
    );
    assert!(s.contains("setup (S)"), "setup pending hint in the list");
    a.stop_all_sessions();
    let s = buf_string(&draw(&a, 180, 40));
    assert!(!s.contains("· project ●"), "dot gone without live setup");
    assert!(s.contains("project config (setup)"));
    assert!(s.contains("setup pending"), "middle column setup warning");
    assert!(s.contains("(none) — p play"), "no cards hint");
    assert!(s.contains("· project"), "statusline shows project row");
}

#[test]
fn board_empty_backlog_asks_to_select() {
    let dir = TmpDir::new("empty");
    let a = app_with(&dir, &[]);
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("Board · 0"));
    assert!(s.contains("select a task (j/k)"), "middle column hint");
    assert!(s.contains("Details"), "right panel placeholder");
}

#[test]
fn board_right_panel_shows_full_task_detail() {
    let dir = TmpDir::new("detailpane");
    write_task(
        &dir,
        "TASK-010",
        "---\nid: TASK-010\ntype: impl\nstatus: review\nrfcs: [RFC-001]\nadrs: [ADR-002]\nprs:\n  - repo: org/x\n    pr: 7\n    branch: feat/x\n  - repo: org/y\n    pr: 0\n    branch: feat/y\ndeferred:\n  - polish later\nconstraints:\n  - text: no sql in handlers\n    enforce: review\n  - text: never touch src/legacy/\n    enforce: hook\n---\n\n## Objective\n\nsome **bold** goal\n\n| col | val |\n|-----|-----|\n| a \\| b | ab supercalifragilisticexpialidocious |\n",
    );
    let a = app_with(&dir, &[]);
    assert_eq!(a.tasks.len(), 1);
    let s = buf_string(&draw(&a, 140, 45));
    assert!(s.contains("TASK-010 · Enter expand"), "panel title");
    assert!(s.contains("RFCs") && s.contains("RFC-001"));
    assert!(s.contains("ADRs") && s.contains("ADR-002"));
    assert!(s.contains("PR #7"), "created PR");
    assert!(s.contains("PR not created"), "pr == 0 placeholder");
    assert!(s.contains("Constraints"));
    assert!(s.contains("[hook]") && s.contains("[review]"));
    assert!(s.contains("Deferred") && s.contains("polish later"));
    assert!(s.contains("REVIEW"), "status label uppercased");
    // middle column shows the first PR line
    assert!(s.contains("PR #7 · feat/x"));
}

#[test]
fn board_middle_column_pr_zero_and_body() {
    let dir = TmpDir::new("przero");
    let a = app_with(&dir, &[("TASK-001", "wip")]);
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("no PR · feat/TASK-001"));
    assert!(s.contains("Items"));
    assert!(s.contains("Objective"), "task body rendered below Items");
}

#[test]
fn chat_fullscreen_uses_whole_body() {
    let dir = TmpDir::new("fullscreen");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.chat_fullscreen = true;
    let s = buf_string(&draw(&a, 120, 40));
    // no cards: fullscreen falls back to the task detail panel
    assert!(s.contains("TASK-001 · Enter expand"));
    assert!(!s.contains("Board · 1"), "task list hidden in fullscreen");
}

// --- render: sessions ---------------------------------------------------------

#[test]
fn session_pane_renders_live_focused_and_hints() {
    let dir = TmpDir::new("pty");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    assert_eq!(a.board_focus, BoardFocus::Chat);
    a.sessions[0].parser.process(b"hello-from-pty");

    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("hello-from-pty"), "pty content visible");
    assert!(s.contains("typing in claude"), "focused hint");
    assert!(s.contains("play"), "session card label");
    assert!(s.contains("active ·"), "live card age");

    a.pending_prefix = true;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("Ctrl+G…"), "prefix hint");
    a.pending_prefix = false;

    a.board_focus = BoardFocus::Cards;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("l/→ enter chat"), "unfocused hint");
    a.stop_all_sessions();
}

#[test]
fn session_pane_renders_closed_history() {
    let dir = TmpDir::new("closed");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    if let Some(s) = &mut a.sessions[0].session {
        let _ = s.kill();
    }
    a.sessions[0].session = None;
    a.sessions[0].finished = true;
    a.board_focus = BoardFocus::Cards;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("Session closed (history)"));
    assert!(s.contains("closed"), "card shows closed state");
}

#[test]
fn session_card_age_formats_minutes_and_hours() {
    let dir = TmpDir::new("age");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u2", &dir);
    a.sessions[0].last_activity = SystemTime::now() - Duration::from_secs(2 * 60);
    a.sessions[1].last_activity = SystemTime::now() - Duration::from_secs(2 * 3600 + 120);
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("2m"), "minutes form");
    assert!(s.contains("2h2m"), "hours form");
    a.stop_all_sessions();
}

#[test]
fn session_pane_tolerates_missing_session_and_remote_client() {
    let dir = TmpDir::new("bogus");
    let a = app_with(&dir, &[("TASK-001", "wip")]);

    // local mode with no selected session: empty chat panel, no panic
    let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
    term.draw(|f| tui::render_session_pane(f, Some(&a), Rect::new(0, 0, 60, 10), false))
        .unwrap();
    assert!(buf_string(&term).contains("chat"));

    // socket client (no App): explicit placeholder, focused and not
    let mut term = Terminal::new(TestBackend::new(80, 10)).unwrap();
    term.draw(|f| tui::render_session_pane(f, None, Rect::new(0, 0, 80, 10), true))
        .unwrap();
    let s = buf_string(&term);
    assert!(s.contains("Session panel not available"));
    assert!(s.contains("Esc back to cards"));
    let mut term = Terminal::new(TestBackend::new(80, 10)).unwrap();
    term.draw(|f| tui::render_session_pane(f, None, Rect::new(0, 0, 80, 10), false))
        .unwrap();
    assert!(buf_string(&term).contains("Session panel not available"));
}

#[test]
fn remote_render_shows_placeholder_for_live_session() {
    let dir = TmpDir::new("remote");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    a.sessions[0].parser.process(b"hello-from-pty");

    let s = buf_string(&draw_remote(&a, 120, 40));
    assert!(
        !s.contains("hello-from-pty"),
        "pty content never renders remotely"
    );
    assert!(s.contains("Session panel not available"));
    assert!(s.contains("play"), "session card still listed");
    a.stop_all_sessions();
}

// --- render: overlays --------------------------------------------------------

#[test]
fn detail_overlay_renders_and_ignores_missing_task() {
    let dir = TmpDir::new("detail-ov");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.open_detail();
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("TASK-001 · j/k scroll · Esc close"));
    assert!(s.contains("Objective"));

    // overlay flag without a task: render must not panic nor draw the overlay
    a.project_selected = true;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(!s.contains("j/k scroll · Esc close"));
}

#[test]
fn doc_view_overlay_renders_markdown() {
    let dir = TmpDir::new("docview");
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("guide.md"),
        "# Title\n\nprose with **bold**\n\n| h |\n|---|\n| v |\n",
    )
    .unwrap();
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;
    a.open_doc();
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("guide.md · j/k scroll · Esc close"));
    assert!(s.contains("Title"));
    assert!(s.contains("╭"), "table border rendered");
}

#[test]
fn picker_overlay_lists_projects() {
    let dir = TmpDir::new("picker");
    let dir2 = TmpDir::new("picker-b");
    fs::create_dir_all(dir2.0.join(".backlog")).unwrap();
    let mk = |name: &str, d: &TmpDir| config::Project {
        name: name.into(),
        root: d.0.clone(),
        backlog: d.0.join(".backlog"),
        docs: d.0.join("docs"),
        work_dir: d.0.join(".jaum"),
        repos: Vec::new(),
    };
    let cfg = config::Config {
        projects: vec![mk("alpha", &dir), mk("beta", &dir2)],
    };
    let mut a = App::new(cfg, 0).unwrap();
    a.executor = ClaudeExecutor::with_bin("cat");
    a.open_picker();
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("Projects · Enter switch · Esc close"));
    assert!(s.contains("alpha") && s.contains("beta"));
    assert!(has_bg(&draw(&a, 120, 40), SEL_BG_COLOR), "picker selection");
}

#[test]
fn job_overlay_renders_log_styles_running_and_done() {
    let dir = TmpDir::new("job");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    let logs = [
        "→ Bash ls -la",
        "→ Build",
        "∴ thinking about it",
        "— all done",
        "[fable] model output",
        "[unclosed bracket line",
        "plain progress line",
    ];
    let (job, _tx) = fake_job(&logs, false, true, 0);
    a.job = Some(job);
    a.job_overlay = true;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("init · running…"));
    assert!(s.contains("j/k scroll · Esc close"));
    assert!(s.contains("Bash") && s.contains("ls -la"));
    assert!(s.contains("thinking about it"));
    assert!(s.contains("all done"));
    assert!(s.contains("[fable]") && s.contains("model output"));
    assert!(s.contains("[unclosed bracket line"));
    assert!(s.contains("plain progress line"));

    // finished + paused scroll variant (scroll clamps, nav hint changes)
    let mut long_logs: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
    long_logs.push("x".repeat(300)); // exercises the wrap estimate
    let refs: Vec<&str> = long_logs.iter().map(String::as_str).collect();
    let (job, _tx2) = fake_job(&refs, true, false, 9);
    a.job = Some(job);
    let s = buf_string(&draw(&a, 100, 30));
    assert!(s.contains("init · done"));
    assert!(s.contains("G live"), "paused hint offers going live");
}

#[test]
fn review_keys_are_unbound_and_gone_from_the_footer() {
    let dir = TmpDir::new("no-review-keys");
    let mut a = app_with(&dir, &[("TASK-001", "review")]);
    let before = a.status_msg.clone();
    for k in ['r', 'R', 'H'] {
        tui::handle_key(&mut a, ch(k));
    }
    assert!(
        a.job.is_none(),
        "no job starts from the removed review keys"
    );
    assert_eq!(a.status_msg, before, "r/R/H must be no-ops");

    let s = buf_string(&draw(&a, 120, 40));
    assert!(!s.contains("R review"), "review chat key gone from footer");
    assert!(!s.contains("H handoff"), "handoff key gone from footer");
}

#[test]
fn job_overlay_flag_without_job_draws_nothing() {
    let dir = TmpDir::new("job-none");
    let mut a = app_with(&dir, &[]);
    a.job_overlay = true;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(!s.contains("running…"));
}

#[test]
fn toast_renders_after_status_change() {
    let dir = TmpDir::new("toast");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.status_msg = "something happened".into();
    a.tick_toast();
    assert_eq!(a.active_toast(), Some("something happened"));
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("● something happened"));

    // very long message clamps to the terminal width without panicking
    a.status_msg = "y".repeat(500);
    a.rearm_toast();
    let _ = draw(&a, 80, 24);
}

// --- render: docs tab --------------------------------------------------------

#[test]
fn docs_tab_renders_groups_preview_and_colors() {
    let dir = TmpDir::new("docs");
    let docs = dir.0.join("docs");
    for (path, body) in [
        ("readme.md", "# Root\nplain"),
        (
            "rfcs/001-a.md",
            "# RFC\n\n| a | b |\n|---|---|\n| supercalifragilisticexpialidocious | x |\n\n| a |  |\n|---|---|\n| b |  |\n",
        ),
        ("adr/002-b.md", "# ADR"),
        ("prd/p.md", "# PRD"),
        ("design/d.md", "# Design"),
        (
            "notes/n.md",
            "```\ncode | pipe\n```\n| lonely row without separator\n",
        ),
    ] {
        let full = docs.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, body).unwrap();
    }
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("Docs · 6"));
    for group in ["(root)", "RFCS", "ADR", "PRD", "DESIGN", "NOTES"] {
        assert!(s.contains(group), "doc group {group}");
    }
    assert!(s.contains("J/K scroll · Enter expand"), "preview title");

    // preview of the rfc doc: bordered table with a hard-split long word
    a.docs_selected = a.docs.iter().position(|d| d.contains("rfcs/")).unwrap();
    let s = buf_string(&draw(&a, 90, 40));
    assert!(s.contains("001-a.md"));
    assert!(s.contains("╭") && s.contains("╰"));

    // the fenced doc renders as prose, not as a table
    a.docs_selected = a.docs.iter().position(|d| d.contains("notes/")).unwrap();
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("code | pipe"));
    assert!(s.contains("lonely row"));
}

#[test]
fn docs_tab_empty_shows_add_docs_hint() {
    let dir = TmpDir::new("docs-empty");
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("(no .md in"));
    assert!(s.contains("Add RFCs/ADRs/specs here"));
}

#[test]
fn docs_preview_with_out_of_range_selection() {
    let dir = TmpDir::new("docs-oob");
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.md"), "# A").unwrap();
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;
    a.docs_selected = 99;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("preview"), "placeholder preview title");
}

// --- render: statusline input + small sizes ----------------------------------

#[test]
fn statusline_shows_input_prompt_per_kind() {
    let dir = TmpDir::new("input-line");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    for (kind, label) in [
        (InputKind::Defer, "defer"),
        (InputKind::Convention, "convention"),
        (InputKind::NewTask, "new task"),
        (InputKind::InitPath, "init (project path)"),
    ] {
        a.input = Some((kind, "abc".into()));
        let s = buf_string(&draw(&a, 120, 40));
        assert!(s.contains(&format!("{label} ▸")), "prompt label {label}");
        assert!(s.contains("abc"));
        assert!(s.contains("Enter confirm · Esc cancel"));
    }
}

#[test]
fn tiny_terminals_render_without_panicking() {
    let dir = TmpDir::new("tiny");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.open_detail();
    for (w, h) in [(2, 2), (4, 3), (10, 5), (120, 8), (30, 40)] {
        let term = draw(&a, w, h);
        assert_eq!(term.backend().buffer().area, Rect::new(0, 0, w, h));
    }
    a.close_detail();
    a.tab = Tab::Docs;
    let _ = draw(&a, 12, 6);
}

#[test]
fn statusline_reflects_selection_focus_tab_and_overlap() {
    let dir = TmpDir::new("statusline");
    let mut a = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    let text = |a: &App| tui::statusline_text(&crate::snapshot::build_snapshot(a));

    let s = text(&a);
    assert!(s.contains("[Board]"));
    assert!(s.contains("TASK-001"));
    assert!(s.contains("feat/TASK-001"));
    assert!(s.contains("focus"), "tasks focus hint");
    assert!(s.contains("overlap"), "two wip tasks on the same repo");

    a.board_focus = BoardFocus::Cards;
    assert!(text(&a).contains("Enter chat"));
    a.board_focus = BoardFocus::Chat;
    assert!(text(&a).contains("Ctrl+G"));

    a.project_selected = true;
    assert!(text(&a).contains("· project"));

    a.tab = Tab::Docs;
    let s = text(&a);
    assert!(s.contains("[Docs]"));
    assert!(!s.contains("focus"), "no board hints on the docs tab");
}

#[test]
fn header_highlights_docs_tab() {
    let dir = TmpDir::new("header");
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;
    let s = buf_string(&draw(&a, 120, 40));
    assert!(s.contains("2 Docs"));
    assert!(s.contains("[Docs]"), "statusline follows the tab");
}

// --- handle_key: overlays ----------------------------------------------------

#[test]
fn job_overlay_keys_scroll_and_dismiss() {
    let dir = TmpDir::new("job-keys");
    let mut a = app_with(&dir, &[]);
    let (job, _tx) = fake_job(&["l1", "l2"], false, true, 0);
    a.job = Some(job);
    a.job_overlay = true;

    tui::handle_key(&mut a, ch('j'));
    let j = a.job.as_ref().unwrap();
    assert!(
        !j.follow && j.scroll == 1,
        "j scrolls down and stops follow"
    );
    tui::handle_key(&mut a, key(KeyCode::Up));
    assert_eq!(a.job.as_ref().unwrap().scroll, 0);
    tui::handle_key(&mut a, key(KeyCode::Down));
    tui::handle_key(&mut a, ch('g'));
    assert_eq!(a.job.as_ref().unwrap().scroll, 0, "g jumps to top");
    tui::handle_key(&mut a, ch('G'));
    assert!(a.job.as_ref().unwrap().follow, "G resumes follow");
    tui::handle_key(&mut a, key(KeyCode::Home));
    tui::handle_key(&mut a, key(KeyCode::End));
    tui::handle_key(&mut a, key(KeyCode::F(1))); // ignored
    assert!(a.job_overlay);

    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert!(!a.job_overlay, "Esc closes the overlay");
    assert!(a.job.is_some(), "unfinished job keeps running");

    a.job.as_mut().unwrap().finished = true;
    a.job_overlay = true;
    tui::handle_key(&mut a, ch('q'));
    assert!(!a.job_overlay && a.job.is_none(), "q drops a finished job");
}

#[test]
fn picker_keys_navigate_and_close() {
    let dir = TmpDir::new("picker-keys");
    let dir2 = TmpDir::new("picker-keys-b");
    fs::create_dir_all(dir2.0.join(".backlog")).unwrap();
    let mk = |name: &str, d: &TmpDir| config::Project {
        name: name.into(),
        root: d.0.clone(),
        backlog: d.0.join(".backlog"),
        docs: d.0.join("docs"),
        work_dir: d.0.join(".jaum"),
        repos: Vec::new(),
    };
    let cfg = config::Config {
        projects: vec![mk("alpha", &dir), mk("beta", &dir2)],
    };
    let mut a = App::new(cfg, 0).unwrap();
    a.executor = ClaudeExecutor::with_bin("cat");

    tui::handle_key(&mut a, ch('P'));
    assert!(a.project_picker);
    tui::handle_key(&mut a, ch('j'));
    assert_eq!(a.picker_selected, 1);
    tui::handle_key(&mut a, ch('k'));
    assert_eq!(a.picker_selected, 0);
    tui::handle_key(&mut a, key(KeyCode::F(1))); // ignored
    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert!(!a.project_picker);
    tui::handle_key(&mut a, ch('P'));
    tui::handle_key(&mut a, ch('q'));
    assert!(!a.project_picker);
    tui::handle_key(&mut a, ch('P'));
    tui::handle_key(&mut a, key(KeyCode::Enter)); // same project: no switch
    assert!(!a.project_picker);
    assert_eq!(a.project_name(), "alpha");
}

#[test]
fn detail_and_doc_overlay_keys() {
    let dir = TmpDir::new("ov-keys");
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.md"), "# A").unwrap();
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);

    a.open_detail();
    tui::handle_key(&mut a, ch('j'));
    assert_eq!(a.detail_scroll, 1);
    tui::handle_key(&mut a, ch('k'));
    assert_eq!(a.detail_scroll, 0);
    tui::handle_key(&mut a, key(KeyCode::F(1)));
    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert!(!a.detail_open);
    a.open_detail();
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(!a.detail_open, "Enter also closes the detail");
    a.open_detail();
    tui::handle_key(&mut a, ch('q'));
    assert!(!a.detail_open);

    a.tab = Tab::Docs;
    a.open_doc();
    tui::handle_key(&mut a, ch('j'));
    assert_eq!(a.doc_scroll, 1);
    tui::handle_key(&mut a, ch('k'));
    assert_eq!(a.doc_scroll, 0);
    tui::handle_key(&mut a, key(KeyCode::F(1)));
    tui::handle_key(&mut a, ch('q'));
    assert!(!a.doc_open);
    a.open_doc();
    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert!(!a.doc_open);
}

#[test]
fn input_capture_edits_and_submits() {
    let dir = TmpDir::new("input-keys");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);

    tui::handle_key(&mut a, ch('c'));
    assert!(matches!(a.input, Some((InputKind::Convention, _))));
    tui::handle_key(&mut a, ch('h'));
    tui::handle_key(&mut a, ch('i'));
    tui::handle_key(&mut a, key(KeyCode::Backspace));
    assert_eq!(a.input.as_ref().unwrap().1, "h");
    tui::handle_key(&mut a, key(KeyCode::F(1))); // ignored inside input
    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert!(a.input.is_none(), "Esc cancels the capture");

    // convention: type and confirm
    tui::handle_key(&mut a, ch('c'));
    for c in "team rule".chars() {
        tui::handle_key(&mut a, ch(c));
    }
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.input.is_none());
    assert_eq!(a.status_msg, "convention added");
    assert!(a.conventions.contains("team rule"));

    // quick task
    tui::handle_key(&mut a, ch('n'));
    for c in "do a thing".chars() {
        tui::handle_key(&mut a, ch(c));
    }
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.status_msg.contains("task created"));

    // defer needs a selected task ('d' guard)
    a.select_last();
    tui::handle_key(&mut a, ch('d'));
    assert!(matches!(a.input, Some((InputKind::Defer, _))));
    for c in "later".chars() {
        tui::handle_key(&mut a, ch(c));
    }
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.status_msg.contains("deferred"), "msg: {}", a.status_msg);

    // empty submissions cancel without side effects (and without spawning)
    a.input = Some((InputKind::InitPath, String::new()));
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.status_msg.contains("init cancelled"));
}

// --- handle_key: navigation --------------------------------------------------

#[test]
fn board_navigation_focus_and_quit() {
    let dir = TmpDir::new("nav");
    let mut a = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    // TASK-001 gets a card that exists but isn't live (a closed history entry).
    open_cat(
        &mut a,
        SessionKind::Play,
        Some("TASK-001"),
        "u-closed",
        &dir,
    );
    if let Some(s) = &mut a.sessions[0].session {
        let _ = s.kill();
    }
    a.sessions[0].session = None;
    a.sessions[0].finished = true;
    a.board_focus = BoardFocus::Tasks;
    a.refresh().unwrap();

    tui::handle_key(&mut a, key(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Docs);
    tui::handle_key(&mut a, ch('1'));
    assert_eq!(a.tab, Tab::Board);
    tui::handle_key(&mut a, ch('2'));
    assert_eq!(a.tab, Tab::Docs);
    tui::handle_key(&mut a, ch('h')); // Docs: tab prev
    assert_eq!(a.tab, Tab::Board);

    tui::handle_key(&mut a, ch('j'));
    assert_eq!(a.selected, 1);
    tui::handle_key(&mut a, ch('k'));
    assert_eq!(a.selected, 0);
    tui::handle_key(&mut a, ch('g'));
    assert!(a.project_selected, "g goes to the project row");
    tui::handle_key(&mut a, ch('G'));
    assert!(!a.project_selected && a.selected == 1);
    tui::handle_key(&mut a, key(KeyCode::Home));
    tui::handle_key(&mut a, key(KeyCode::End));

    // focus: Tasks -> Cards (TASK-001 has a closed history card) -> stays (not live)
    tui::handle_key(&mut a, ch('g'));
    tui::handle_key(&mut a, ch('j')); // back to first task
    tui::handle_key(&mut a, ch('l'));
    assert_eq!(a.board_focus, BoardFocus::Cards);
    tui::handle_key(&mut a, ch('l'));
    assert_eq!(a.board_focus, BoardFocus::Cards, "history card is not live");
    tui::handle_key(&mut a, ch('j')); // card_next within cards focus
    tui::handle_key(&mut a, ch('k'));
    tui::handle_key(&mut a, key(KeyCode::Right));
    tui::handle_key(&mut a, ch('h'));
    assert_eq!(a.board_focus, BoardFocus::Tasks);
    tui::handle_key(&mut a, key(KeyCode::Left));
    assert_eq!(a.board_focus, BoardFocus::Tasks);

    // Enter on tasks focus opens the detail; 'o' too
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.detail_open);
    tui::handle_key(&mut a, key(KeyCode::Esc));
    tui::handle_key(&mut a, ch('o'));
    assert!(a.detail_open);
    tui::handle_key(&mut a, key(KeyCode::Esc));

    tui::handle_key(&mut a, ch('z'));
    assert!(a.chat_fullscreen);
    tui::handle_key(&mut a, ch('z'));
    assert!(!a.chat_fullscreen);

    tui::handle_key(&mut a, key(KeyCode::F(2))); // unmapped: no-op
    tui::handle_key(&mut a, ch('q'));
    assert!(a.should_quit);
    a.should_quit = false;
    tui::handle_key(&mut a, ctrl('c'));
    assert!(a.should_quit, "Ctrl+C quits outside the chat");
}

#[test]
fn docs_navigation_and_preview_scroll() {
    let dir = TmpDir::new("docs-nav");
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.md"), "# A").unwrap();
    fs::write(docs.join("b.md"), "# B").unwrap();
    let mut a = app_with(&dir, &[]);
    a.tab = Tab::Docs;

    tui::handle_key(&mut a, ch('j'));
    assert_eq!(a.docs_selected, 1);
    tui::handle_key(&mut a, ch('k'));
    assert_eq!(a.docs_selected, 0);
    tui::handle_key(&mut a, ch('J'));
    assert_eq!(a.doc_scroll, 1, "Shift+J scrolls the preview");
    tui::handle_key(&mut a, ch('K'));
    assert_eq!(a.doc_scroll, 0);
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert!(a.doc_open, "Enter expands the doc");
    tui::handle_key(&mut a, key(KeyCode::Esc));
    tui::handle_key(&mut a, ch('l')); // Docs: next tab
    assert_eq!(a.tab, Tab::Board);
}

#[test]
fn action_keys_are_safe_without_target_or_with_running_job() {
    let dir = TmpDir::new("actions");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.select_first(); // project row: no target task

    tui::handle_key(&mut a, ch('p'));
    assert_eq!(a.status_msg, "no task selected");
    tui::handle_key(&mut a, ch('r'));
    assert_eq!(a.status_msg, "no task selected");
    tui::handle_key(&mut a, ch('R'));
    tui::handle_key(&mut a, ch('H'));
    assert_eq!(a.status_msg, "no task selected");
    tui::handle_key(&mut a, ch('f'));
    assert!(a.sessions.is_empty());
    tui::handle_key(&mut a, ch('d')); // guard: no selected task, no input
    assert!(a.input.is_none());

    // background jobs guarded by an already-running job (never spawns)
    let (job, _tx) = fake_job(&["busy"], false, true, 0);
    a.job = Some(job);
    a.job_overlay = false;
    tui::handle_key(&mut a, ch('i'));
    tui::handle_key(&mut a, ch('a'));
    assert_eq!(a.job.as_ref().unwrap().logs, vec!["busy".to_string()]);
    a.job = None;

    tui::handle_key(&mut a, ch('I'));
    assert!(matches!(a.input, Some((InputKind::InitPath, _))));
    tui::handle_key(&mut a, key(KeyCode::Esc));
    tui::handle_key(&mut a, ch('e'));
    assert!(a.edit_request);
    a.edit_request = false;

    // setup opens a chat via the stub executor
    tui::handle_key(&mut a, ch('S'));
    assert_eq!(a.status_msg, "setup: chat opened");
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.sessions[0].kind, SessionKind::Setup);
    a.stop_all_sessions();
}

#[test]
fn play_key_focuses_existing_live_session() {
    let dir = TmpDir::new("play-key");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    a.board_focus = BoardFocus::Tasks;
    tui::handle_key(&mut a, ch('p'));
    assert_eq!(a.board_focus, BoardFocus::Chat);
    assert!(a.status_msg.contains("is already open"));

    // Enter on a live card enters the chat
    a.board_focus = BoardFocus::Cards;
    tui::handle_key(&mut a, key(KeyCode::Enter));
    assert_eq!(a.board_focus, BoardFocus::Chat);
    a.stop_all_sessions();
}

// --- handle_key: chat focus --------------------------------------------------

#[test]
fn chat_prefix_commands() {
    let dir = TmpDir::new("prefix");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u2", &dir);
    assert_eq!(a.board_focus, BoardFocus::Chat);

    tui::handle_key(&mut a, ctrl('g'));
    assert!(a.pending_prefix);
    tui::handle_key(&mut a, ch('2'));
    assert_eq!(a.tab, Tab::Docs);
    assert!(!a.pending_prefix);
    a.tab = Tab::Board;

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('1'));
    assert_eq!(a.tab, Tab::Board);

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('z'));
    assert!(a.chat_fullscreen);
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('z'));
    assert!(!a.chat_fullscreen);

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('g')); // literal Ctrl+G forwarded to the pty
    assert!(!a.pending_prefix);

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, key(KeyCode::F(3))); // unmapped prefix key
    assert!(!a.pending_prefix);
    assert_eq!(a.board_focus, BoardFocus::Chat);

    // n/p between two live cards keeps the chat
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('n'));
    assert_eq!(a.board_focus, BoardFocus::Chat);
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('p'));
    assert_eq!(a.board_focus, BoardFocus::Chat);

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('h'));
    assert_eq!(a.board_focus, BoardFocus::Cards);
    a.board_focus = BoardFocus::Chat;

    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('q'));
    assert!(a.should_quit);
    a.should_quit = false;

    // x removes the current session; f finishes the remaining one
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('x'));
    assert_eq!(a.sessions.len(), 1);
    a.board_focus = BoardFocus::Chat;
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('f'));
    assert!(a.sessions[0].finished);
    assert!(a.status_msg.contains("finished"));
    a.stop_all_sessions();
}

#[test]
fn chat_prefix_next_prev_leave_chat_on_dead_card() {
    let dir = TmpDir::new("prefix-dead");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    // newest first: cards [0]=u3, [1]=u2, [2]=u1. Kill u1 and u3 so the live
    // session (u2) sits between two dead cards.
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u2", &dir);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u3", &dir);
    for i in [0, 2] {
        if let Some(s) = &mut a.sessions[i].session {
            let _ = s.kill();
        }
        a.sessions[i].session = None;
        a.sessions[i].finished = true;
    }

    // from the live card (1), moving prev lands on a dead card -> Cards focus
    a.card_selected = 1;
    a.board_focus = BoardFocus::Chat;
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('p'));
    assert_eq!(a.board_focus, BoardFocus::Cards);
    assert_eq!(a.card_selected, 0);

    // moving next also lands on a dead card -> Cards focus
    a.card_selected = 1;
    a.board_focus = BoardFocus::Chat;
    tui::handle_key(&mut a, ctrl('g'));
    tui::handle_key(&mut a, ch('n'));
    assert_eq!(a.board_focus, BoardFocus::Cards);
    assert_eq!(a.card_selected, 2);
    a.stop_all_sessions();
}

#[test]
fn chat_forwards_keys_and_esc_leaves() {
    let dir = TmpDir::new("forward");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    assert_eq!(a.board_focus, BoardFocus::Chat);

    // all of these are swallowed by the pty (no app-level effect)
    for k in [
        ch('a'),
        ctrl('x'),
        ctrl('c'), // forwarded, NOT quit
        ctrl('1'), // ctrl over a non-letter falls back to the plain char
        key(KeyCode::Enter),
        key(KeyCode::Tab),
        key(KeyCode::Backspace),
        key(KeyCode::Up),
        key(KeyCode::Down),
        key(KeyCode::Left),
        key(KeyCode::Right),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::SHIFT), // modified Esc forwarded
        key(KeyCode::F(6)),                               // encodes to nothing
    ] {
        tui::handle_key(&mut a, k);
    }
    assert!(!a.should_quit, "Ctrl+C in chat goes to the pty");
    assert_eq!(a.board_focus, BoardFocus::Chat);

    tui::handle_key(&mut a, key(KeyCode::Esc));
    assert_eq!(
        a.board_focus,
        BoardFocus::Cards,
        "plain Esc leaves the chat"
    );
    a.stop_all_sessions();
}

#[test]
fn key_to_bytes_encodes_terminal_input() {
    assert_eq!(tui::key_to_bytes(ch('a')), b"a".to_vec());
    assert_eq!(tui::key_to_bytes(ctrl('c')), vec![0x03]);
    assert_eq!(tui::key_to_bytes(ctrl('Z')), vec![0x1a]);
    assert_eq!(tui::key_to_bytes(ctrl('1')), b"1".to_vec());
    assert_eq!(tui::key_to_bytes(key(KeyCode::Enter)), vec![b'\r']);
    assert_eq!(tui::key_to_bytes(key(KeyCode::Backspace)), vec![0x7f]);
    assert_eq!(tui::key_to_bytes(key(KeyCode::Tab)), vec![b'\t']);
    assert_eq!(tui::key_to_bytes(key(KeyCode::Esc)), vec![0x1b]);
    assert_eq!(tui::key_to_bytes(key(KeyCode::Up)), b"\x1b[A".to_vec());
    assert_eq!(tui::key_to_bytes(key(KeyCode::Down)), b"\x1b[B".to_vec());
    assert_eq!(tui::key_to_bytes(key(KeyCode::Right)), b"\x1b[C".to_vec());
    assert_eq!(tui::key_to_bytes(key(KeyCode::Left)), b"\x1b[D".to_vec());
    assert!(tui::key_to_bytes(key(KeyCode::F(1))).is_empty());
}

// --- mouse + pty geometry ----------------------------------------------------

fn mouse(kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: mods,
    }
}

#[test]
fn mouse_scrolls_history_when_claude_has_no_mouse_mode() {
    let dir = TmpDir::new("mouse");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    // fill the scrollback so the wheel has history to reveal
    for i in 0..120 {
        a.sessions[0]
            .parser
            .process(format!("line {i}\r\n").as_bytes());
    }
    let area = tui::session_term_area(&a, 120, 40);
    assert!(area.width > 0 && area.height > 0);
    let (cx, cy) = (area.x + 1, area.y + 1);

    tui::handle_mouse(
        &mut a,
        mouse(MouseEventKind::ScrollUp, cx, cy, KeyModifiers::NONE),
        120,
        40,
    );
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 3);
    tui::handle_mouse(
        &mut a,
        mouse(MouseEventKind::ScrollDown, cx, cy, KeyModifiers::NONE),
        120,
        40,
    );
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 0);
    // click without mouse mode: ignored
    tui::handle_mouse(
        &mut a,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            cx,
            cy,
            KeyModifiers::NONE,
        ),
        120,
        40,
    );
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 0);

    // outside the pane: ignored
    tui::handle_mouse(
        &mut a,
        mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE),
        120,
        40,
    );
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 0);

    // outside the chat focus: ignored
    a.board_focus = BoardFocus::Cards;
    tui::handle_mouse(
        &mut a,
        mouse(MouseEventKind::ScrollUp, cx, cy, KeyModifiers::NONE),
        120,
        40,
    );
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 0);
    a.stop_all_sessions();
}

#[test]
fn mouse_in_sgr_mode_is_forwarded_to_the_pty() {
    let dir = TmpDir::new("mouse-sgr");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    // claude enabled mouse reporting (SGR encoding)
    a.sessions[0].parser.process(b"\x1b[?1002h\x1b[?1006h");
    let area = tui::session_term_area(&a, 120, 40);
    let (cx, cy) = (area.x + 2, area.y + 2);

    for (kind, mods) in [
        (MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
        (MouseEventKind::Up(MouseButton::Right), KeyModifiers::NONE),
        (
            MouseEventKind::Drag(MouseButton::Middle),
            KeyModifiers::NONE,
        ),
        (MouseEventKind::ScrollUp, KeyModifiers::SHIFT),
        (MouseEventKind::ScrollDown, KeyModifiers::ALT),
        (MouseEventKind::ScrollLeft, KeyModifiers::CONTROL),
        (MouseEventKind::ScrollRight, KeyModifiers::NONE),
        (MouseEventKind::Moved, KeyModifiers::NONE), // encodes to nothing
    ] {
        tui::handle_mouse(&mut a, mouse(kind, cx, cy, mods), 120, 40);
    }
    // forwarded to the pty, never to the local scrollback
    assert_eq!(a.sessions[0].parser.screen().scrollback(), 0);
    a.stop_all_sessions();
}

#[test]
fn mouse_without_session_is_a_noop() {
    let dir = TmpDir::new("mouse-none");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    a.board_focus = BoardFocus::Chat; // forced: no session exists
    let area = tui::session_term_area(&a, 120, 40);
    tui::handle_mouse(
        &mut a,
        mouse(
            MouseEventKind::ScrollUp,
            area.x + 1,
            area.y + 1,
            KeyModifiers::NONE,
        ),
        120,
        40,
    );
    assert!(a.sessions.is_empty());
}

#[test]
fn sync_pty_to_resizes_only_the_selected_live_session() {
    let dir = TmpDir::new("sync");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);

    // no session selected: no-op
    tui::sync_pty_to(&mut a, 120, 40);

    open_cat(&mut a, SessionKind::Play, Some("TASK-001"), "u1", &dir);
    let before = a.sessions[0].parser.screen().size();
    tui::sync_pty_to(&mut a, 120, 40);
    let after = a.sessions[0].parser.screen().size();
    assert_ne!(before, after, "parser resized to the chat pane");
    let area = tui::session_term_area(&a, 120, 40);
    assert_eq!(after, (area.height, area.width));

    // idempotent when the size already matches
    tui::sync_pty_to(&mut a, 120, 40);
    assert_eq!(a.sessions[0].parser.screen().size(), after);

    // fullscreen uses the whole body
    a.chat_fullscreen = true;
    tui::sync_pty_to(&mut a, 120, 40);
    let full = a.sessions[0].parser.screen().size();
    assert!(full.1 > after.1, "fullscreen pane is wider");
    let full_area = tui::session_term_area(&a, 120, 40);
    assert_eq!(full, (full_area.height, full_area.width));

    // degenerate terminal: bail out before a 0x0 resize
    tui::sync_pty_to(&mut a, 5, 4);
    assert_eq!(a.sessions[0].parser.screen().size(), full);
    a.stop_all_sessions();
}
