use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_core::Status;

// The app/config modules are private to the binary; we re-exercise the pure logic
// by recompiling them as part of the test crate. (app.rs references
// `crate::config`, `crate::protocol`, `crate::snapshot`, `crate::session_event`
// and `crate::sidecar`, so those must be declared here too.)
#[path = "../src/app.rs"]
#[allow(dead_code)]
mod app;
#[path = "../src/config.rs"]
#[allow(dead_code)]
mod config;
#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;
#[path = "../src/session_event.rs"]
#[allow(dead_code)]
mod session_event;
#[path = "../src/sidecar.rs"]
#[allow(dead_code)]
mod sidecar;
#[path = "../src/snapshot.rs"]
#[allow(dead_code)]
mod snapshot;

use app::{App, STATUS_ORDER, Tab, sort_for_board};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-app-{tag}-{}-{n}", std::process::id()));
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
        ci_poll_secs: None,
        projects: vec![project],
    };
    App::new(cfg, 0).unwrap()
}

#[test]
fn tab_cycles_around() {
    assert_eq!(Tab::Board.next(), Tab::Docs);
    assert_eq!(Tab::Docs.next(), Tab::Board);
    assert_eq!(Tab::from_index(1), Tab::Docs);
    assert_eq!(Tab::Docs.index(), 1);
}

#[test]
fn vim_nav_select_first_last() {
    let dir = TmpDir::new("vim");
    let mut app = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "wip"),
            ("TASK-003", "wip"),
        ],
    );
    app.select_last();
    assert_eq!(app.selected, 2);
    app.select_first();
    assert_eq!(app.selected, 0);
}

#[test]
fn sort_for_board_groups_by_canonical_status() {
    let dir = TmpDir::new("sort");
    let app = app_with(
        &dir,
        &[
            ("TASK-001", "backlog"),
            ("TASK-002", "wip"),
            ("TASK-003", "review"),
        ],
    );
    // wip comes before review, which comes before backlog
    let ids: Vec<&str> = app.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["TASK-002", "TASK-003", "TASK-001"]);
    assert_eq!(STATUS_ORDER[0], Status::Wip);
}

#[test]
fn sort_for_board_is_stable_by_id() {
    let dir = TmpDir::new("sortid");
    let app = app_with(&dir, &[("TASK-003", "wip"), ("TASK-001", "wip")]);
    let ids: Vec<&str> = app.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["TASK-001", "TASK-003"]);
    let _ = sort_for_board(Vec::new()); // doesn't panic on empty
}

#[test]
fn detail_opens_only_with_selected_task_and_closes() {
    let dir = TmpDir::new("detail");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(!app.detail_open);
    app.open_detail();
    assert!(app.detail_open);
    app.detail_scroll_down();
    assert_eq!(app.detail_scroll, 1);
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 0);
    app.close_detail();
    assert!(!app.detail_open);

    // with no task selected it doesn't open
    let dir2 = TmpDir::new("detail-empty");
    let mut empty = app_with(&dir2, &[]);
    empty.open_detail();
    assert!(!empty.detail_open);
}

#[test]
fn navigation_respects_bounds() {
    let dir = TmpDir::new("nav");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    assert_eq!(app.selected, 0);
    assert!(!app.project_selected);
    app.select_prev(); // from the top it moves up to the · project row
    assert!(app.project_selected);
    app.select_next(); // back to the first task
    assert!(!app.project_selected);
    assert_eq!(app.selected, 0);
    app.select_next();
    assert_eq!(app.selected, 1);
    app.select_next(); // doesn't go past the end
    assert_eq!(app.selected, 1);
}

#[test]
fn defer_creates_new_backlog_and_refreshes() {
    let dir = TmpDir::new("defer");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let before = app.tasks.len();
    app.defer("extract date parser");
    assert_eq!(app.tasks.len(), before + 1);
    assert!(app.status_msg.contains("deferred"));
}

#[test]
fn add_convention_appends_to_file_and_reloads() {
    let dir = TmpDir::new("conv");
    let mut app = app_with(&dir, &[]);
    app.add_convention("don't reference RFC numbers in comments");
    assert!(app.conventions.contains("don't reference RFC numbers"));
    let on_disk = fs::read_to_string(&app.conventions_path).unwrap();
    assert!(on_disk.contains("- don't reference RFC numbers in comments"));
}

#[test]
fn new_task_quick_creates_backlog_with_objective() {
    let dir = TmpDir::new("newtask");
    let mut app = app_with(&dir, &[]);
    app.new_task_quick("analyze the rest of the project looking for RFC refs");
    assert_eq!(app.tasks.len(), 1);
    assert!(
        app.tasks[0]
            .body
            .contains("analyze the rest of the project")
    );
    assert!(app.status_msg.contains("task created"));
}

fn cat_session() -> jaum_adapters::Session {
    use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};
    ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap()
}

#[test]
fn multi_session_per_task_navigates_and_closes() {
    use app::{BoardFocus, SessionKind};
    let dir = TmpDir::new("multi");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(a.sessions.is_empty());

    // two sessions of the SAME task (play + review) become two of its cards.
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-1".into(),
        dir.0.clone(),
    );
    a.open_session(
        SessionKind::Review,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-2".into(),
        dir.0.clone(),
    );
    assert_eq!(a.sessions.len(), 2);
    // opens in the Board, the just-created chat focused, owner task selected
    assert_eq!(a.tab, Tab::Board);
    assert_eq!(a.board_focus, BoardFocus::Chat);
    assert_eq!(a.selected_task().unwrap().id, "TASK-001");
    assert_eq!(a.task_cards().len(), 2);

    // the focused session is the just-opened one (uuid-2)
    let cur = a.current_session_idx().unwrap();
    assert_eq!(a.sessions[cur].claude_session_id, "uuid-2");

    // navigate the cards (there and back)
    a.card_next();
    a.card_prev();
    assert_eq!(a.current_session_idx().unwrap(), cur);

    // finish: marks the card's session as done, keeps it in the list
    a.finish_selected_session();
    assert_eq!(a.sessions.len(), 2);
    assert!(a.status_msg.contains("finished"));

    // close the current card: removes it from the list
    a.close_selected_session();
    assert_eq!(a.sessions.len(), 1);

    a.stop_all_sessions();
    assert!(a.sessions.is_empty());
}

#[test]
fn sessions_most_recent_on_top() {
    use app::SessionKind;
    let dir = TmpDir::new("sort");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    for uuid in ["s1", "s2", "s3"] {
        a.open_session(
            SessionKind::Play,
            Some("TASK-001".into()),
            cat_session(),
            Vec::new(),
            uuid.into(),
            dir.0.clone(),
        );
    }
    // the last opened (s3) stays on top
    assert_eq!(a.sessions[0].claude_session_id, "s3");

    // recent activity on an old session (s1) brings it to the top
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    a.sessions
        .iter_mut()
        .find(|e| e.claude_session_id == "s1")
        .unwrap()
        .last_activity = later;
    a.sort_sessions();
    assert_eq!(a.sessions[0].claude_session_id, "s1");
}

#[test]
fn play_selected_focuses_existing_live_session_without_duplicating() {
    use app::SessionKind;
    let dir = TmpDir::new("play-dup");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    // there's already a live play session for the selected task
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-live".into(),
        dir.0.clone(),
    );
    a.tab = Tab::Board;
    assert_eq!(a.sessions.len(), 1);

    a.play_selected();
    // no duplicate: focused the existing session (chat in the Board)
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.tab, Tab::Board);
    assert_eq!(a.board_focus, app::BoardFocus::Chat);
    assert!(
        a.status_msg.contains("is already open"),
        "msg: {}",
        a.status_msg
    );
}

#[test]
fn pr_sync_targets_only_picks_task_with_live_play_and_pr_zero() {
    use app::SessionKind;
    let dir = TmpDir::new("prsync");
    // the fixture already creates the task with PrLink { repo: org/x, pr: 0, branch: feat/<id> }
    let mut a = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);

    // no live play session -> no targets
    assert!(a.pr_sync_targets().is_empty());

    // open a live play only for TASK-001
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-001".into(),
        dir.0.clone(),
    );
    let targets = a.pr_sync_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0],
        (
            "TASK-001".to_string(),
            "org/x".to_string(),
            "feat/TASK-001".to_string()
        )
    );

    // a finished session no longer counts
    a.sessions[0].finished = true;
    assert!(a.pr_sync_targets().is_empty());
}

#[test]
fn finished_session_does_not_count_as_live() {
    use app::SessionKind;
    let dir = TmpDir::new("dead-session");
    // NON-wip task: the only reason to be "active" would be the live session.
    let mut a = app_with(&dir, &[("TASK-001", "backlog")]);
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-dead".into(),
        dir.0.clone(),
    );
    assert!(a.sessions[0].is_live());
    assert!(a.active_task_ids().contains(&"TASK-001".to_string()));

    // simulate claude having exited (Ctrl+C/Ctrl+D): drain would mark `finished`.
    a.sessions[0].finished = true;
    assert!(
        !a.sessions[0].is_live(),
        "finished session (dead process) is not live, even with session=Some"
    );
    // and no longer counts as an active task
    assert!(!a.active_task_ids().contains(&"TASK-001".to_string()));
}

fn write_review(dir: &TmpDir, id: &str, clean: bool) {
    let body = if clean {
        format!("---\ntask: {id}\nfindings: []\nconstraints: []\n---\nok\n")
    } else {
        format!(
            "---\ntask: {id}\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints: []\n---\nbug\n"
        )
    };
    fs::write(dir.0.join(format!(".backlog/{id}.review.md")), body).unwrap();
}

#[test]
fn project_holds_setup_sessions() {
    use app::SessionKind;
    let dir = TmpDir::new("project");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);

    // a setup session (no task) focuses the · project row
    a.open_session(
        SessionKind::Setup,
        None,
        cat_session(),
        Vec::new(),
        "uuid-setup".into(),
        dir.0.clone(),
    );
    assert!(a.project_selected);
    assert_eq!(a.selected_task().map(|t| t.id.clone()), None);
    // the · project row's cards are the setup sessions
    assert_eq!(a.task_cards().len(), 1);
    let cur = a.current_session_idx().unwrap();
    assert_eq!(a.sessions[cur].kind, SessionKind::Setup);

    // leaving the · project row for the first task clears the setup cards
    a.select_next();
    assert!(!a.project_selected);
    assert!(a.task_cards().is_empty());
}

#[test]
fn verdict_card_appears_only_for_task_with_review() {
    use app::BoardCard;
    let dir = TmpDir::new("verdict-card");
    let mut a = app_with(&dir, &[("TASK-001", "review"), ("TASK-002", "review")]);
    // only 001 has a saved review
    write_review(&dir, "TASK-001", true);
    a.refresh().unwrap();

    // TASK-001 (with review) -> has a verdict card
    a.selected = a.tasks.iter().position(|t| t.id == "TASK-001").unwrap();
    assert!(a.task_cards().contains(&BoardCard::Verdict));

    // TASK-002 (without review) -> no verdict card
    a.selected = a.tasks.iter().position(|t| t.id == "TASK-002").unwrap();
    assert!(!a.task_cards().contains(&BoardCard::Verdict));
}

#[test]
fn handoff_selected_sends_findings_to_play() {
    use app::{SessionEntry, SessionKind};
    let dir = TmpDir::new("handoff");
    let mut a = app_with(&dir, &[("TASK-001", "review")]);
    // write a DIRTY review (1 finding + 1 failed constraint)
    fs::write(
        dir.0.join(".backlog/TASK-001.review.md"),
        "---\ntask: TASK-001\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints:\n  - text: rule\n    verdict: failed\n---\nbody\n",
    )
    .unwrap();
    a.refresh().unwrap();
    // a live play session with a turn in flight: the handoff queues behind it
    let log = session_event::SessionLog::new(&dir.0, "uuid-h");
    let mut entry = SessionEntry::sidecar(
        SessionKind::Play,
        Some("TASK-001".into()),
        Vec::new(),
        "uuid-h".into(),
        dir.0.clone(),
        log,
    );
    let (_tx, rx) = std::sync::mpsc::channel();
    {
        let chat = entry.chat.as_mut().unwrap();
        chat.rx = Some(rx);
        chat.request_id = Some("uuid-h#1".into());
        chat.turn_seq = 1;
    }
    a.sessions.push(entry);

    a.handoff_selected();
    assert_eq!(a.tab, Tab::Board);
    assert_eq!(a.board_focus, app::BoardFocus::Chat);
    assert!(a.status_msg.contains("sent"), "msg: {}", a.status_msg);
    assert_eq!(a.sessions[0].chat.as_ref().unwrap().queued.len(), 1);
}

#[test]
fn handoff_selected_without_review_warns() {
    let dir = TmpDir::new("handoff-none");
    let mut a = app_with(&dir, &[("TASK-001", "review")]);
    a.handoff_selected();
    assert!(a.status_msg.contains("run review"));
}

#[test]
fn parallelism_badge_relative_to_active_task() {
    let dir = TmpDir::new("parallel");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();
    // declared conflict between 001 and 002 (in repo org/x)
    fs::write(
        work.join("parallel.json"),
        r#"{"conflicts":[{"a":"TASK-001","b":"TASK-002","repo":"org/x","reason":"both edit src/x.rs"}]}"#,
    )
    .unwrap();
    // 001 is wip (active); 002 and 003 idle
    let mut a = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "backlog"),
            ("TASK-003", "backlog"),
        ],
    );
    a.refresh().unwrap();
    assert!(
        a.parallel.is_some(),
        "parallel.json should have been loaded"
    );

    // 002 conflicts with the active 001
    let c = a.parallel_conflict_with_active("TASK-002");
    assert!(c.is_some());
    let (other, repo, _reason) = c.unwrap();
    assert_eq!(other, "TASK-001");
    assert_eq!(repo, "org/x");
    assert!(!a.is_parallel_safe("TASK-002"), "002 conflicts, not safe");

    // 003 doesn't conflict with the active one -> safe for parallel
    assert!(a.parallel_conflict_with_active("TASK-003").is_none());
    assert!(a.is_parallel_safe("TASK-003"));

    // with no analysis loaded, nothing is marked
    a.parallel = None;
    assert!(a.parallel_conflict_with_active("TASK-002").is_none());
    assert!(!a.is_parallel_safe("TASK-003"));
}

#[test]
fn parallelism_without_active_task_marks_nothing_safe() {
    let dir = TmpDir::new("parallel-idle");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("parallel.json"), r#"{"conflicts":[]}"#).unwrap();
    let mut a = app_with(&dir, &[("TASK-001", "backlog"), ("TASK-002", "backlog")]);
    a.refresh().unwrap();
    // nothing active -> no one to parallelize with -> not "safe"
    assert!(!a.is_parallel_safe("TASK-001"));
    assert!(a.active_task_ids().is_empty());
}

#[test]
fn session_record_roundtrip_serde() {
    use app::{SessionKind, SessionRecord};
    let rec = SessionRecord {
        kind: SessionKind::Play,
        task: Some("TASK-001".into()),
        claude_session_id: "abc-123".into(),
        cwd: PathBuf::from("/tmp/wt"),
        worktrees: vec![("org/x".into(), PathBuf::from("/tmp/wt"))],
        created_ms: 1_700_000_000_000,
        last_activity_ms: 1_700_000_005_000,
        finished: false,
        blocked: false,
    };
    let json = serde_json::to_string(&rec).unwrap();
    // kind serializes in lowercase
    assert!(json.contains("\"play\""), "json: {json}");
    let back: SessionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind, SessionKind::Play);
    assert_eq!(back.claude_session_id, "abc-123");
    assert_eq!(back.cwd, PathBuf::from("/tmp/wt"));
    assert_eq!(back.created_ms, 1_700_000_000_000);
    assert!(!back.finished);
}

#[test]
fn rehydrate_brings_finished_and_missing_cwd_as_history() {
    use app::{SessionKind, SessionRecord};
    let dir = TmpDir::new("rehydrate");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();

    // 1) finished (becomes history even with an existing cwd)
    // 2) live but nonexistent cwd (not resumable -> history)
    let recs = vec![
        SessionRecord {
            kind: SessionKind::Setup,
            task: None,
            claude_session_id: "s-fin".into(),
            cwd: dir.0.clone(),
            worktrees: Vec::new(),
            created_ms: 1_700_000_000_000,
            last_activity_ms: 1_700_000_001_000,
            finished: true,
            blocked: false,
        },
        SessionRecord {
            kind: SessionKind::Play,
            task: Some("TASK-001".into()),
            claude_session_id: "s-gone".into(),
            cwd: dir.0.join("worktree-gone"),
            worktrees: Vec::new(),
            created_ms: 1_700_000_002_000,
            last_activity_ms: 1_700_000_003_000,
            finished: false,
            blocked: false,
        },
    ];
    fs::write(
        work.join("sessions.json"),
        serde_json::to_string(&recs).unwrap(),
    )
    .unwrap();

    // App::new rehydrates at boot
    let a = app_with(&dir, &[("TASK-001", "wip")]);
    assert_eq!(a.sessions.len(), 2, "both should appear in the list");
    // neither is live: finished and missing-cwd fall back to history (no PTY)
    assert!(a.sessions.iter().all(|e| !e.is_live()));
    assert!(a.sessions.iter().all(|e| e.finished));
    assert_eq!(a.sessions[0].kind, SessionKind::Setup);
    assert_eq!(a.sessions[1].claude_session_id, "s-gone");
}

// ingest now goes through the LLM (claude -p) — the deterministic logic (parsing
// structured_output + create_stubs) is tested in crates/flows/tests/ingest.rs.
