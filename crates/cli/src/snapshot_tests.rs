//! Behavior tests for the snapshot builder: the wire view must mirror the
//! `App` domain state, including derived badges and overlays.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::build_snapshot;
use crate::app::{App, SessionKind};
use crate::config;
use crate::protocol::{
    CardView, CheckVerdict, FocusId, InputKind, Intent, ParallelMark, SessionEventKind, StatusId,
    TabId, TaskTypeId,
};
use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-snap-{tag}-{}-{n}", std::process::id()));
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
        "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: org/x\n    pr: 7\n    branch: feat/{id}\nrfcs: [RFC-0001]\ndeferred: [later]\nconstraints:\n  - text: no legacy\n    enforce: hook\n---\n\n## Objective\nbody of {id}\n"
    )
}

fn app_with(dir: &TmpDir, tasks: &[(&str, &str)]) -> App {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for (id, status) in tasks {
        fs::write(backlog.join(format!("{id}.md")), task_md(id, status)).unwrap();
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
        ci_poll_secs: None,
    };
    let mut a = App::new(cfg, 0).unwrap();
    // safety net: any executor spawn in a test becomes `cat`, never `claude`.
    a.executor = ClaudeExecutor::with_bin("cat");
    a
}

fn open_cat(a: &mut App, kind: SessionKind, task: Option<&str>, uuid: &str, cwd: &TmpDir) {
    let session = ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap();
    a.open_session(
        kind,
        task.map(str::to_string),
        session,
        Vec::new(),
        uuid.into(),
        cwd.0.clone(),
    );
}

fn dirty_review_md(id: &str) -> String {
    format!(
        "---\ntask: {id}\nfindings:\n  - file: src/a.rs\n    line: 3\n    message: broken invariant\n    reference: RFC-001\n    severity: blocker\nconstraints:\n  - text: reviewed rule\n    verdict: failed\ncriteria:\n  - text: criterion one\n    verdict: pending\n---\nbody\n"
    )
}

#[test]
fn snapshot_mirrors_tasks_project_and_docs() {
    let dir = TmpDir::new("mirror");
    let docs = dir.0.join("docs/rfcs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("RFC-0001.md"), "# RFC content\n").unwrap();
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    app.refresh().unwrap();

    let snap = build_snapshot(&app);
    assert_eq!(snap.project, "test");
    assert_eq!(snap.tab, TabId::Board);
    assert_eq!(snap.projects.len(), 1);
    assert_eq!(snap.board.tasks.len(), 2);

    let t = &snap.board.tasks[0];
    assert_eq!(t.id, "TASK-001");
    assert_eq!(t.status, StatusId::Wip);
    assert_eq!(t.task_type, TaskTypeId::Impl);
    assert_eq!(t.rfcs, vec!["RFC-0001".to_string()]);
    assert_eq!(t.prs[0].pr, 7);
    assert_eq!(t.prs[0].branch, "feat/TASK-001");
    assert_eq!(t.deferred, vec!["later".to_string()]);
    assert_eq!(t.constraints[0].text, "no legacy");
    assert!(t.body.contains("body of TASK-001"));
    assert!(!t.live_session);
    assert!(t.review.is_none());

    // docs list + preview of the selected doc
    assert_eq!(snap.docs.list, vec!["rfcs/RFC-0001.md".to_string()]);
    assert!(snap.docs.preview.contains("RFC content"));
    assert!(snap.docs.dir.ends_with("docs"));

    // no overlays at boot
    assert!(snap.picker.is_none());
    assert!(snap.input.is_none());
    assert!(snap.job.is_none());
    assert!(!snap.job_overlay);
    assert!(snap.statusline.contains("TASK-001"));
}

#[test]
fn snapshot_reflects_selection_and_overlay_actions() {
    let dir = TmpDir::new("actions");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);

    let before = build_snapshot(&app);
    assert_eq!(before.board.selected, 0);
    assert!(!before.board.detail_open);

    app.apply_intent(Intent::SelectNext);
    let after = build_snapshot(&app);
    assert_eq!(after.board.selected, 1);
    assert_ne!(before, after, "selection change must change the snapshot");

    app.apply_intent(Intent::OpenDetail);
    let detail = build_snapshot(&app);
    assert!(detail.board.detail_open);

    app.apply_intent(Intent::CloseDetail);
    app.apply_intent(Intent::OpenPicker);
    let picker = build_snapshot(&app);
    assert_eq!(picker.picker.map(|p| p.selected), Some(0));

    app.apply_intent(Intent::ClosePicker);
    app.apply_intent(Intent::StartInput {
        kind: InputKind::Convention,
        prefill: String::new(),
    });
    app.apply_intent(Intent::InputChar { ch: 'x' });
    let input = build_snapshot(&app);
    let iv = input.input.unwrap();
    assert_eq!(iv.kind, InputKind::Convention);
    assert_eq!(iv.buffer, "x");
}

#[test]
fn snapshot_carries_review_badge_and_detail() {
    let dir = TmpDir::new("review");
    let app = app_with(&dir, &[("TASK-001", "review")]);
    fs::write(
        dir.0.join(".backlog/TASK-001.review.md"),
        dirty_review_md("TASK-001"),
    )
    .unwrap();

    let snap = build_snapshot(&app);
    let t = &snap.board.tasks[0];
    let badge = t.review.expect("review badge");
    assert!(!badge.clean);
    // 1 finding + 1 failed constraint + 1 pending criterion
    assert_eq!(badge.badge, 3);
    assert_eq!(badge.unmet, 2);

    let review = snap.board.review.expect("review detail");
    assert!(!review.clean);
    assert_eq!(review.blocking, 1);
    assert!(review.findings[0].contains("broken invariant"));
    assert_eq!(review.constraints[0].verdict, CheckVerdict::Failed);
    assert_eq!(review.criteria[0].verdict, CheckVerdict::Pending);

    // verdict card present and dirty
    assert!(
        snap.board
            .cards
            .iter()
            .any(|c| matches!(c, CardView::Verdict { clean: false }))
    );
}

#[test]
fn snapshot_marks_parallel_conflicts_and_safety() {
    let dir = TmpDir::new("parallel");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();
    fs::write(
        work.join("parallel.json"),
        r#"{"conflicts":[{"a":"TASK-001","b":"TASK-002","repo":"org/x","reason":"same file"}]}"#,
    )
    .unwrap();
    let app = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "backlog"),
            ("TASK-003", "backlog"),
        ],
    );

    let snap = build_snapshot(&app);
    let by_id = |id: &str| {
        snap.board
            .tasks
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .parallel
    };
    // the active task carries no mark; the colliding one warns; the free one is safe.
    assert_eq!(by_id("TASK-001"), None);
    assert_eq!(by_id("TASK-002"), Some(ParallelMark::Conflict));
    assert_eq!(by_id("TASK-003"), Some(ParallelMark::Safe));
}

#[test]
fn snapshot_tracks_sessions_cards_focus_and_events() {
    let dir = TmpDir::new("sessions");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);

    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u-play",
        &dir,
    );
    let events = app.take_session_events();
    assert!(matches!(
        &events[0].kind,
        SessionEventKind::Started { task: Some(t), .. } if t == "TASK-001"
    ));
    assert_eq!(events[0].session_id, "u-play");

    let snap = build_snapshot(&app);
    let t = &snap.board.tasks[0];
    assert!(t.live_session);
    assert!(matches!(
        snap.board.cards[0],
        CardView::Session {
            kind: crate::protocol::SessionKind::Play,
            live: true,
            ..
        }
    ));
    // opening a live session focuses its chat
    assert_eq!(snap.board.focus, FocusId::Chat);

    // PTY output surfaces as an Output event on drain (`cat` echoes stdin)
    app.sessions[0]
        .session
        .as_mut()
        .unwrap()
        .write_line("ping")
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut got_output = false;
    while std::time::Instant::now() < deadline {
        app.drain_pty();
        if app
            .take_session_events()
            .iter()
            .any(|e| matches!(&e.kind, SessionEventKind::Output { bytes } if !bytes.is_empty()))
        {
            got_output = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got_output, "drain never emitted the PTY output");

    // finishing the session emits the event and flips the card
    app.finish_selected_session();
    let events = app.take_session_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::Finished))
    );
    let snap = build_snapshot(&app);
    assert!(matches!(
        snap.board.cards[0],
        CardView::Session { live: false, .. }
    ));
}

#[test]
fn snapshot_shows_setup_state_and_toast() {
    let dir = TmpDir::new("setup");
    let mut app = app_with(&dir, &[("TASK-001", "backlog")]);
    // conventions.md missing -> setup pending
    let snap = build_snapshot(&app);
    assert!(snap.board.setup_needed);
    assert!(!snap.board.setup_live);

    open_cat(&mut app, SessionKind::Setup, None, "u-setup", &dir);
    let snap = build_snapshot(&app);
    assert!(snap.board.setup_live);

    // a status change becomes a toast after the tick
    app.status_msg = "something happened".into();
    app.tick_toast();
    let snap = build_snapshot(&app);
    assert_eq!(snap.toast.as_deref(), Some("something happened"));
    app.stop_all_sessions();
}
