//! Behavior tests for the App state machine. They live in-crate (see the
//! `mod app_tests` note in app.rs) so coverage is attributed to src/app.rs.

use std::fs;
use std::path::Path;
use std::process::Command;

use jaum_adapters::{ExecFlags, Executor};

use super::*;
use crate::config::{Project, RepoMap};

static DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("jaum-app-unit-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn task_md(id: &str, status: &str, branch: &str) -> String {
    format!(
        "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: org/x\n    pr: 0\n    branch: {branch}\n---\n\n## Objective\nx\n"
    )
}

fn write_task(backlog: &Path, id: &str, status: &str, branch: &str) {
    fs::create_dir_all(backlog).unwrap();
    fs::write(
        backlog.join(format!("{id}.md")),
        task_md(id, status, branch),
    )
    .unwrap();
}

fn write_unlinked_task(backlog: &Path, id: &str, status: &str) {
    fs::create_dir_all(backlog).unwrap();
    fs::write(
        backlog.join(format!("{id}.md")),
        format!("---\nid: {id}\ntype: impl\nstatus: {status}\nprs: []\n---\n\n## Objective\nx\n"),
    )
    .unwrap();
}

fn project(dir: &Path, repos: Vec<RepoMap>) -> Project {
    Project {
        name: "test".into(),
        root: dir.to_path_buf(),
        backlog: dir.join(".backlog"),
        docs: dir.join("docs"),
        work_dir: dir.join(".jaum"),
        repos,
    }
}

fn app_from(dir: &TmpDir, tasks: &[(&str, &str)], repos: Vec<RepoMap>) -> App {
    let backlog = dir.path().join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    for (id, status) in tasks {
        write_task(&backlog, id, status, &format!("feat/{}", id.to_lowercase()));
    }
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir.path(), repos)],
    };
    App::new(cfg, 0).unwrap()
}

fn app_with(dir: &TmpDir, tasks: &[(&str, &str)]) -> App {
    app_from(dir, tasks, Vec::new())
}

fn cat_session() -> Session {
    ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap()
}

fn open_cat(app: &mut App, kind: SessionKind, task: Option<&str>, uuid: &str, cwd: &Path) {
    app.open_session(
        kind,
        task.map(str::to_string),
        cat_session(),
        Vec::new(),
        uuid.into(),
        cwd.to_path_buf(),
    );
}

fn write_script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// Stub `claude` that prints a single stream-json `result` event carrying the
/// given `structured_output`, whatever the arguments.
fn stub_claude(dir: &Path, structured: &str) -> String {
    write_script(
        dir,
        "claude-stub",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"type\":\"result\",\"structured_output\":{structured}}}'\n"
        ),
    )
}

fn git(args: &[&str], cwd: &Path) {
    let st = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?} failed");
}

fn git_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q"], &repo);
    git(&["commit", "--allow-empty", "-m", "init", "-q"], &repo);
    repo
}

fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not met within timeout");
}

/// Polls `poll_job` until the current job (and anything it chains into) ends.
fn wait_job(app: &mut App) {
    for _ in 0..500 {
        app.poll_job();
        if app.job.as_ref().is_none_or(|j| j.finished) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("job did not finish in time");
}

fn past(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or_else(Instant::now)
}

fn record(kind: SessionKind, task: Option<&str>, uuid: &str, cwd: &Path) -> SessionRecord {
    SessionRecord {
        kind,
        task: task.map(str::to_string),
        claude_session_id: uuid.into(),
        cwd: cwd.to_path_buf(),
        worktrees: Vec::new(),
        created_ms: 1_700_000_000_000,
        last_activity_ms: 1_700_000_001_000,
        finished: false,
    }
}

fn fake_job(kind: JobKind) -> (std::sync::mpsc::Sender<JobMsg>, Job) {
    let (tx, rx) = channel();
    (
        tx,
        Job {
            kind,
            title: "fake".into(),
            task: None,
            logs: Vec::new(),
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        },
    )
}

// --- pure helpers --------------------------------------------------------

#[test]
fn tab_navigation_and_labels() {
    assert_eq!(Tab::all().len(), 2);
    assert_eq!(Tab::Board.title(), "Board");
    assert_eq!(Tab::Docs.title(), "Docs");
    assert_eq!(Tab::Board.index(), 0);
    assert_eq!(Tab::from_index(9), Tab::Docs);
    assert_eq!(Tab::Board.next(), Tab::Docs);
    assert_eq!(Tab::Docs.next(), Tab::Board);
    assert_eq!(Tab::Board.prev(), Tab::Docs);
    assert_eq!(Tab::Docs.prev(), Tab::Board);
}

#[test]
fn status_labels_and_board_order() {
    assert_eq!(status_label(Status::Backlog), "backlog");
    assert_eq!(status_label(Status::Ready), "ready");
    assert_eq!(status_label(Status::Wip), "wip");
    assert_eq!(status_label(Status::Review), "review");
    assert_eq!(status_label(Status::Merged), "merged");

    let mk = |id: &str, status: Status| Task {
        id: id.into(),
        task_type: jaum_core::TaskType::Impl,
        status,
        rfcs: Vec::new(),
        adrs: Vec::new(),
        prs: Vec::new(),
        deferred: Vec::new(),
        constraints: Vec::new(),
        locks: Vec::new(),
        body: String::new(),
        path: None,
    };
    let sorted = sort_for_board(vec![
        mk("TASK-005", Status::Merged),
        mk("TASK-004", Status::Backlog),
        mk("TASK-003", Status::Ready),
        mk("TASK-002", Status::Review),
        mk("TASK-009", Status::Wip),
        mk("TASK-001", Status::Wip),
    ]);
    let ids: Vec<&str> = sorted.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "TASK-001", "TASK-009", "TASK-002", "TASK-003", "TASK-004", "TASK-005"
        ]
    );
}

#[test]
fn list_docs_walks_recursively_and_filters_md() {
    let dir = TmpDir::new("docs-walk");
    let docs = dir.path().join("docs");
    fs::create_dir_all(docs.join("sub")).unwrap();
    fs::write(docs.join("b.md"), "b").unwrap();
    fs::write(docs.join("sub/a.md"), "a").unwrap();
    fs::write(docs.join("notes.txt"), "n").unwrap();
    assert_eq!(
        list_docs(&docs),
        vec!["b.md".to_string(), "sub/a.md".to_string()]
    );
    assert!(list_docs(&dir.path().join("missing")).is_empty());
}

#[test]
fn session_kind_labels() {
    assert_eq!(SessionKind::Play.label(), "play");
    assert_eq!(SessionKind::Review.label(), "review");
    assert_eq!(SessionKind::Setup.label(), "setup");
}

#[test]
fn epoch_ms_roundtrips_and_clamps_before_epoch() {
    let t = from_epoch_ms(1_700_000_000_000);
    assert_eq!(epoch_ms(t), 1_700_000_000_000);
    assert_eq!(epoch_ms(UNIX_EPOCH - Duration::from_secs(5)), 0);
}

#[test]
fn expand_tilde_variants() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_eq!(expand_tilde("~"), home);
    assert_eq!(expand_tilde("~/x/y"), home.join("x/y"));
    assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
}

#[test]
fn setup_needs_any_covers_each_flag() {
    let empty = SetupNeeds::default();
    assert!(!empty.any());
    assert!(
        SetupNeeds {
            unlinked: vec!["TASK-001".into()],
            ..Default::default()
        }
        .any()
    );
    assert!(
        SetupNeeds {
            leaky_branches: vec!["TASK-001".into()],
            ..Default::default()
        }
        .any()
    );
    assert!(
        SetupNeeds {
            conventions_template: true,
            ..Default::default()
        }
        .any()
    );
    assert!(
        SetupNeeds {
            mapping_missing: true,
            ..Default::default()
        }
        .any()
    );
}

// --- session entries ------------------------------------------------------

#[test]
fn session_entry_history_and_record_roundtrip() {
    let dir = TmpDir::new("entry");
    let rec = record(SessionKind::Review, Some("TASK-001"), "u-rec", dir.path());
    let entry = SessionEntry::history(&rec);
    assert!(!entry.is_live());
    assert!(entry.finished);
    assert_eq!(entry.name(), "review · TASK-001");
    let back = entry.to_record();
    assert_eq!(back.claude_session_id, "u-rec");
    assert_eq!(back.created_ms, rec.created_ms);
    assert_eq!(back.last_activity_ms, rec.last_activity_ms);
    assert!(back.finished);

    let setup = SessionEntry::history(&record(SessionKind::Setup, None, "u-s", dir.path()));
    assert_eq!(setup.name(), "setup");
}

#[test]
fn drain_feeds_parser_and_marks_eof() {
    let dir = TmpDir::new("drain");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    let created = app.sessions[0].created;
    app.sessions[0]
        .session
        .as_mut()
        .unwrap()
        .write_line("hello-pty")
        .unwrap();
    wait_until(|| {
        app.drain_pty();
        app.sessions[0]
            .parser
            .screen()
            .contents()
            .contains("hello-pty")
    });
    assert!(app.sessions[0].last_activity >= created);

    // process exit is noticed even before the channel disconnects
    app.sessions[0].session.as_mut().unwrap().kill().unwrap();
    wait_until(|| {
        app.drain_pty();
        app.sessions[0].finished
    });
    assert!(!app.sessions[0].is_live());
    app.stop_all_sessions();
}

#[test]
fn drain_disconnected_channel_marks_finished() {
    let dir = TmpDir::new("drain-eof");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    let (tx, rx) = channel::<Vec<u8>>();
    drop(tx);
    app.sessions[0].rx = Some(rx);
    app.sessions[0].drain();
    assert!(app.sessions[0].finished);
    app.stop_all_sessions();
}

#[test]
fn drain_try_wait_detects_dead_process_with_live_channel() {
    let dir = TmpDir::new("drain-wait");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    // keep the sender alive so the loop always ends on Empty, not Disconnected
    let (_tx, rx) = channel::<Vec<u8>>();
    app.sessions[0].rx = Some(rx);
    app.sessions[0].session.as_mut().unwrap().kill().unwrap();
    wait_until(|| {
        app.sessions[0].drain();
        app.sessions[0].finished
    });
    app.stop_all_sessions();
}

#[test]
fn sort_sessions_breaks_activity_ties_by_seq_and_keeps_focus() {
    let dir = TmpDir::new("sort-seq");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let rec = record(SessionKind::Play, Some("TASK-001"), "a", dir.path());
    let a = SessionEntry::history(&rec);
    let b = SessionEntry::history(&SessionRecord {
        claude_session_id: "b".into(),
        ..rec.clone()
    });
    app.sessions.push(a);
    app.sessions.push(b);
    app.board_focus = BoardFocus::Cards;
    app.card_selected = 0; // the "a" card
    app.sort_sessions();
    // same timestamps: the later-created entry (higher seq) wins the top
    assert_eq!(app.sessions[0].claude_session_id, "b");
    // focus followed the "a" session to its new card position
    assert_eq!(app.card_selected, 1);
}

#[test]
fn sort_sessions_short_list_is_noop() {
    let dir = TmpDir::new("sort-one");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-001"),
        "solo",
        dir.path(),
    )));
    app.card_selected = 0;
    app.sort_sessions();
    assert_eq!(app.sessions.len(), 1);
}

// --- construction, refresh, rehydration -----------------------------------

#[test]
fn new_fails_on_invalid_project_index() {
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: Vec::new(),
    };
    assert!(App::new(cfg, 0).is_err());
}

#[test]
fn refresh_clamps_selection_and_docs_cursor() {
    let dir = TmpDir::new("refresh");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    fs::write(dir.path().join("docs/a.md"), "a").unwrap();
    app.selected = 99;
    app.docs_selected = 99;
    app.refresh().unwrap();
    assert_eq!(app.selected, 1);
    assert_eq!(app.docs_selected, 0);
    assert_eq!(app.docs, vec!["a.md".to_string()]);
}

#[test]
fn refresh_reports_overlap_between_wip_tasks_in_same_repo() {
    let dir = TmpDir::new("overlap");
    let app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    assert!(!app.overlaps.is_empty());
    assert!(app.statusline().contains("overlap"));
}

#[test]
fn boot_ignores_invalid_sessions_file() {
    let dir = TmpDir::new("bad-sessions");
    let work = dir.path().join(".jaum");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("sessions.json"), "not json").unwrap();
    let app = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(app.sessions.is_empty());
    assert!(app.load_session_records().is_empty());
}

#[test]
fn boot_rehydrates_records_as_history() {
    let dir = TmpDir::new("boot-hist");
    let work = dir.path().join(".jaum");
    fs::create_dir_all(&work).unwrap();
    let mut fin = record(SessionKind::Setup, None, "s-fin", dir.path());
    fin.finished = true;
    let gone = record(
        SessionKind::Play,
        Some("TASK-001"),
        "s-gone",
        &dir.path().join("missing-cwd"),
    );
    fs::write(
        work.join("sessions.json"),
        serde_json::to_string(&vec![fin, gone]).unwrap(),
    )
    .unwrap();
    let app = app_with(&dir, &[("TASK-001", "wip")]);
    assert_eq!(app.sessions.len(), 2);
    assert!(app.sessions.iter().all(|e| !e.is_live()));
}

#[test]
fn rehydrate_one_resumes_each_kind_with_the_executor() {
    let dir = TmpDir::new("rehydrate-live");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.executor = ClaudeExecutor::with_bin("cat");

    let play = app.rehydrate_one(record(
        SessionKind::Play,
        Some("TASK-001"),
        "u-p",
        dir.path(),
    ));
    assert!(play.is_live());
    // resume preserves the persisted clock instead of resetting it
    assert_eq!(play.created, from_epoch_ms(1_700_000_000_000));
    assert_eq!(play.last_activity, from_epoch_ms(1_700_000_001_000));

    let review = app.rehydrate_one(record(
        SessionKind::Review,
        Some("TASK-001"),
        "u-r",
        dir.path(),
    ));
    assert!(review.is_live());

    let setup = app.rehydrate_one(record(SessionKind::Setup, None, "u-s", dir.path()));
    assert!(setup.is_live());

    for mut e in [play, review, setup] {
        if let Some(s) = &mut e.session {
            let _ = s.kill();
        }
    }
}

#[test]
fn rehydrate_one_falls_back_to_history_when_resume_fails() {
    let dir = TmpDir::new("rehydrate-fail");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.executor = ClaudeExecutor::with_bin("cat");
    // play/review records without a task cannot resume
    let p = app.rehydrate_one(record(SessionKind::Play, None, "u-p", dir.path()));
    assert!(!p.is_live());
    let r = app.rehydrate_one(record(SessionKind::Review, None, "u-r", dir.path()));
    assert!(!r.is_live());
}

// --- conventions, detail, picker, projects --------------------------------

#[test]
fn edit_request_and_reload_conventions() {
    let dir = TmpDir::new("conv-reload");
    let mut app = app_with(&dir, &[]);
    assert!(!app.edit_request);
    app.request_edit_conventions();
    assert!(app.edit_request);
    fs::write(&app.conventions_path, "- rule\n").unwrap();
    app.reload_conventions();
    assert_eq!(app.conventions, "- rule\n");
}

#[test]
fn detail_overlay_lifecycle() {
    let dir = TmpDir::new("detail");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.open_detail();
    assert!(app.detail_open);
    app.detail_scroll_down();
    app.detail_scroll_down();
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 1);
    app.close_detail();
    assert!(!app.detail_open);

    let dir2 = TmpDir::new("detail-none");
    let mut empty = app_with(&dir2, &[]);
    empty.open_detail();
    assert!(!empty.detail_open);
}

#[test]
fn project_name_falls_back_when_index_invalid() {
    let dir = TmpDir::new("pname");
    let mut app = app_with(&dir, &[]);
    assert_eq!(app.project_name(), "test");
    app.current = 42;
    assert_eq!(app.project_name(), "?");
    assert_eq!(app.backlog_path(), PathBuf::new());
}

#[test]
fn load_project_switches_and_stops_sessions() {
    let dir1 = TmpDir::new("proj-a");
    let dir2 = TmpDir::new("proj-b");
    let backlog2 = dir2.path().join(".backlog");
    write_task(&backlog2, "TASK-009", "wip", "feat/other");
    let mut p2 = project(dir2.path(), Vec::new());
    p2.name = "second".into();
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir1.path(), Vec::new()), p2],
    };
    fs::create_dir_all(dir1.path().join(".backlog")).unwrap();
    let mut app = App::new(cfg, 0).unwrap();
    open_cat(&mut app, SessionKind::Setup, None, "u-set", dir1.path());
    assert_eq!(app.sessions.len(), 1);

    app.load_project(1);
    assert_eq!(app.current, 1);
    assert_eq!(app.project_name(), "second");
    assert!(app.sessions.is_empty());
    assert_eq!(app.tasks.len(), 1);
    assert!(app.status_msg.contains("second"));

    // out-of-range index is ignored
    app.load_project(7);
    assert_eq!(app.current, 1);
}

#[test]
fn picker_navigation_and_confirm() {
    let dir1 = TmpDir::new("pick-a");
    let dir2 = TmpDir::new("pick-b");
    fs::create_dir_all(dir1.path().join(".backlog")).unwrap();
    fs::create_dir_all(dir2.path().join(".backlog")).unwrap();
    let mut p2 = project(dir2.path(), Vec::new());
    p2.name = "two".into();
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir1.path(), Vec::new()), p2],
    };
    let mut app = App::new(cfg, 0).unwrap();

    app.open_picker();
    assert!(app.project_picker);
    assert_eq!(app.picker_selected, 0);
    app.picker_next();
    app.picker_next(); // clamped at the last project
    assert_eq!(app.picker_selected, 1);
    app.picker_prev();
    app.picker_prev(); // clamped at zero
    assert_eq!(app.picker_selected, 0);
    app.close_picker();
    assert!(!app.project_picker);

    // confirming the current project does not reload it
    app.open_picker();
    app.confirm_picker();
    assert_eq!(app.current, 0);
    // confirming another project switches
    app.open_picker();
    app.picker_next();
    app.confirm_picker();
    assert_eq!(app.current, 1);
    assert_eq!(app.project_name(), "two");
}

// --- board: cards, focus, selection ---------------------------------------

#[test]
fn task_cards_lists_sessions_and_verdict() {
    let dir = TmpDir::new("cards");
    let mut app = app_with(&dir, &[("TASK-001", "review"), ("TASK-002", "review")]);
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    )));
    fs::write(
        dir.path().join(".backlog/TASK-001.review.md"),
        "---\ntask: TASK-001\nfindings: []\nconstraints: []\n---\nok\n",
    )
    .unwrap();
    app.selected = app.tasks.iter().position(|t| t.id == "TASK-001").unwrap();
    let cards = app.task_cards();
    assert_eq!(cards, vec![BoardCard::Session(0), BoardCard::Verdict]);

    app.card_selected = 1;
    assert_eq!(app.selected_card(), Some(BoardCard::Verdict));
    assert!(app.current_session_idx().is_none());
    assert!(!app.selected_card_is_live());

    // card cursor bounds
    app.card_next();
    assert_eq!(app.card_selected, 1);
    app.card_prev();
    app.card_prev();
    assert_eq!(app.card_selected, 0);

    // no cards at all: cursor movement is a no-op and there is no card
    app.selected = app.tasks.iter().position(|t| t.id == "TASK-002").unwrap();
    assert!(app.task_cards().is_empty());
    app.card_selected = 0;
    app.card_next();
    assert!(app.selected_card().is_none());
}

#[test]
fn task_cards_for_project_row_and_empty_backlog() {
    let dir = TmpDir::new("cards-proj");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Setup,
        None,
        "u-set",
        dir.path(),
    )));
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-001"),
        "u-play",
        dir.path(),
    )));
    app.project_selected = true;
    assert_eq!(app.task_cards(), vec![BoardCard::Session(0)]);
    assert!(app.selected_task().is_none());
    assert!(app.target_task_id().is_none());

    let dir2 = TmpDir::new("cards-empty");
    let empty = app_with(&dir2, &[]);
    assert!(empty.task_cards().is_empty());
}

#[test]
fn focus_moves_between_panels_only_when_allowed() {
    let dir = TmpDir::new("focus");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    // no cards: focus stays on tasks
    app.focus_right();
    assert_eq!(app.board_focus, BoardFocus::Tasks);

    // a history card allows Cards focus, but not Chat (not live)
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-001"),
        "u-h",
        dir.path(),
    )));
    app.focus_right();
    assert_eq!(app.board_focus, BoardFocus::Cards);
    app.focus_right();
    assert_eq!(app.board_focus, BoardFocus::Cards);

    // a live card unlocks the chat
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u-live",
        dir.path(),
    );
    app.board_focus = BoardFocus::Cards;
    assert!(app.selected_card_is_live());
    app.focus_right();
    assert_eq!(app.board_focus, BoardFocus::Chat);

    app.focus_left();
    assert_eq!(app.board_focus, BoardFocus::Cards);
    app.focus_left();
    assert_eq!(app.board_focus, BoardFocus::Tasks);
    app.focus_left();
    assert_eq!(app.board_focus, BoardFocus::Tasks);
    app.stop_all_sessions();
}

#[test]
fn selection_moves_through_project_row_and_bounds() {
    let dir = TmpDir::new("select");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    app.card_selected = 3;
    app.select_prev(); // top task -> project row
    assert!(app.project_selected);
    assert_eq!(app.card_selected, 0);
    app.select_prev(); // already at the top
    assert!(app.project_selected);
    app.select_next();
    assert!(!app.project_selected);
    assert_eq!(app.selected, 0);
    app.select_next();
    app.select_next(); // clamped at the end
    assert_eq!(app.selected, 1);
    app.select_first();
    assert!(app.project_selected);
    app.select_last();
    assert!(!app.project_selected);
    assert_eq!(app.selected, 1);

    let dir2 = TmpDir::new("select-empty");
    let mut empty = app_with(&dir2, &[]);
    empty.select_next();
    empty.select_last();
    assert_eq!(empty.selected, 0);
}

#[test]
fn review_badge_counts_findings_and_unmet_items() {
    let dir = TmpDir::new("badge");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    assert!(app.load_review("TASK-001").is_none());
    assert!(app.review_badge("TASK-001").is_none());
    fs::write(
        dir.path().join(".backlog/TASK-001.review.md"),
        "---\ntask: TASK-001\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints:\n  - text: rule\n    verdict: failed\n---\nbody\n",
    )
    .unwrap();
    app.refresh().unwrap();
    assert_eq!(app.review_badge("TASK-001"), Some(2));
}

#[test]
fn statusline_reflects_selection_focus_and_tab() {
    let dir = TmpDir::new("statusline");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let s = app.statusline();
    assert!(s.contains("[Board]"));
    assert!(s.contains("TASK-001"));
    assert!(s.contains("feat/task-001"));
    assert!(s.contains("focus"));

    app.board_focus = BoardFocus::Cards;
    assert!(app.statusline().contains("Enter chat"));
    app.board_focus = BoardFocus::Chat;
    assert!(app.statusline().contains("Ctrl+G"));

    app.project_selected = true;
    assert!(app.statusline().contains("· project"));

    app.tab = Tab::Docs;
    let s = app.statusline();
    assert!(s.contains("[Docs]"));
    assert!(!s.contains("focus"));
}

// --- parallelism ----------------------------------------------------------

#[test]
fn parallel_conflicts_relative_to_active_tasks() {
    let dir = TmpDir::new("parallel");
    let work = dir.path().join(".jaum");
    fs::create_dir_all(&work).unwrap();
    fs::write(
        work.join("parallel.json"),
        r#"{"conflicts":[{"a":"TASK-001","b":"TASK-002","repo":"org/x","reason":"same file"}]}"#,
    )
    .unwrap();
    let mut app = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "backlog"),
            ("TASK-003", "backlog"),
        ],
    );
    assert!(app.parallel.is_some());
    assert_eq!(app.active_task_ids(), vec!["TASK-001".to_string()]);

    let (other, repo, reason) = app.parallel_conflict_with_active("TASK-002").unwrap();
    assert_eq!((other.as_str(), repo.as_str()), ("TASK-001", "org/x"));
    assert_eq!(reason, "same file");
    assert!(!app.is_parallel_safe("TASK-002"));
    // the active task never conflicts with itself
    assert!(app.parallel_conflict_with_active("TASK-001").is_none());
    assert!(app.parallel_conflict_with_active("TASK-003").is_none());
    assert!(app.is_parallel_safe("TASK-003"));

    app.parallel = None;
    assert!(app.parallel_conflict_with_active("TASK-002").is_none());
    assert!(!app.is_parallel_safe("TASK-003"));
}

#[test]
fn active_tasks_include_live_play_sessions_once() {
    let dir = TmpDir::new("active");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-002"),
        "u2",
        dir.path(),
    );
    let ids = app.active_task_ids();
    assert_eq!(ids.iter().filter(|i| i.as_str() == "TASK-001").count(), 1);
    assert!(ids.contains(&"TASK-002".to_string()));
    app.stop_all_sessions();
}

// --- docs tab ---------------------------------------------------------------

#[test]
fn docs_navigation_and_viewer() {
    let dir = TmpDir::new("docs-nav");
    let mut app = app_with(&dir, &[]);
    // nothing to open with an empty list
    app.open_doc();
    assert!(!app.doc_open);
    app.docs_next();
    assert_eq!(app.docs_selected, 0);

    fs::write(dir.path().join("docs/a.md"), "a").unwrap();
    fs::write(dir.path().join("docs/b.md"), "b").unwrap();
    app.refresh().unwrap();
    app.docs_next();
    app.docs_next(); // clamped
    assert_eq!(app.docs_selected, 1);
    app.docs_prev();
    app.docs_prev(); // clamped
    assert_eq!(app.docs_selected, 0);
    app.open_doc();
    assert!(app.doc_open);
    app.doc_scroll_down();
    app.doc_scroll_down();
    app.doc_scroll_up();
    assert_eq!(app.doc_scroll, 1);
    app.close_doc();
    assert!(!app.doc_open);
}

// --- setup ------------------------------------------------------------------

#[test]
fn setup_needs_reports_missing_pieces() {
    let dir = TmpDir::new("needs");
    let backlog = dir.path().join(".backlog");
    write_unlinked_task(&backlog, "TASK-001", "backlog");
    write_task(&backlog, "TASK-002", "backlog", "feat/task-002"); // leaks the id
    let mut app = app_with(&dir, &[]);
    app.refresh().unwrap();
    let needs = app.setup_needs();
    assert_eq!(needs.unlinked, vec!["TASK-001".to_string()]);
    assert_eq!(needs.leaky_branches, vec!["TASK-002".to_string()]);
    assert!(needs.conventions_template);
    assert!(needs.mapping_missing);
    assert!(app.setup_needed());
}

#[test]
fn setup_needs_clean_when_everything_is_configured() {
    let dir = TmpDir::new("needs-ok");
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/clean-branch");
    fs::write(dir.path().join("conventions.md"), "- always test\n").unwrap();
    fs::write(dir.path().join("setup.md"), "map\n").unwrap();
    let app = app_with(&dir, &[]);
    assert!(!app.setup_needed());
}

#[test]
fn home_defaults_when_conventions_path_has_no_parent() {
    let dir = TmpDir::new("home");
    let mut app = app_with(&dir, &[]);
    assert_eq!(app.home(), dir.path());
    app.conventions_path = PathBuf::from("/");
    assert_eq!(app.home(), PathBuf::new());
}

#[test]
fn setup_start_opens_chat_or_reports_failure() {
    let dir = TmpDir::new("setup-start");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.executor = ClaudeExecutor::with_bin("cat");
    app.setup_start();
    assert_eq!(app.status_msg, "setup: chat opened");
    assert_eq!(app.sessions[0].kind, SessionKind::Setup);
    assert!(app.project_selected);
    app.stop_all_sessions();

    app.executor = ClaudeExecutor::with_bin("/nonexistent-bin-jaum-test");
    app.setup_start();
    assert!(app.status_msg.contains("setup failed"));
}

// --- play / review / handoff / finish --------------------------------------

#[test]
fn play_selected_requires_a_task_and_a_mapped_repo() {
    let dir = TmpDir::new("play-none");
    let mut app = app_with(&dir, &[]);
    app.play_selected();
    assert_eq!(app.status_msg, "no task selected");

    let dir2 = TmpDir::new("play-unmapped");
    let mut app2 = app_with(&dir2, &[("TASK-001", "backlog")]);
    app2.executor = ClaudeExecutor::with_bin("cat");
    app2.play_selected();
    assert!(
        app2.status_msg.contains("play failed"),
        "{}",
        app2.status_msg
    );
}

#[test]
fn play_selected_creates_worktree_session_and_close_cleans_up() {
    let dir = TmpDir::new("play-ok");
    let repo = git_repo(dir.path());
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/nice-work");
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo.clone(),
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.executor = ClaudeExecutor::with_bin("cat");
    app.gh = Gh::with_bin("false");

    app.play_selected();
    assert!(
        app.status_msg.contains("play started"),
        "{}",
        app.status_msg
    );
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.sessions[0].kind, SessionKind::Play);
    let wt = repo.with_file_name("repo.worktrees").join("feat-nice-work");
    assert!(wt.exists());
    assert_eq!(app.tasks[0].status, Status::Wip);

    app.close_selected_session();
    assert!(app.sessions.is_empty());
    assert!(!wt.exists());
}

#[test]
fn play_selected_focuses_existing_live_session() {
    let dir = TmpDir::new("play-dup");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u-live",
        dir.path(),
    );
    app.play_selected();
    assert_eq!(app.sessions.len(), 1);
    assert!(app.status_msg.contains("already open"));
    assert_eq!(app.board_focus, BoardFocus::Chat);
    app.stop_all_sessions();
}

#[test]
fn review_selected_opens_read_only_session() {
    let dir = TmpDir::new("review-ok");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    app.executor = ClaudeExecutor::with_bin("cat");
    app.review_selected();
    assert!(
        app.status_msg.contains("read-only review of TASK-001"),
        "{}",
        app.status_msg
    );
    assert_eq!(app.sessions[0].kind, SessionKind::Review);
    app.stop_all_sessions();
}

#[test]
fn review_selected_handles_failure_and_missing_selection() {
    let dir = TmpDir::new("review-fail");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    app.executor = ClaudeExecutor::with_bin("/nonexistent-bin-jaum-test");
    app.review_selected();
    assert!(app.status_msg.contains("review failed"));

    let dir2 = TmpDir::new("review-none");
    let mut empty = app_with(&dir2, &[]);
    empty.review_selected();
    assert_eq!(empty.status_msg, HINT);
}

fn write_dirty_review(dir: &TmpDir, id: &str) {
    fs::write(
        dir.path().join(format!(".backlog/{id}.review.md")),
        format!(
            "---\ntask: {id}\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints:\n  - text: rule\n    verdict: failed\n---\nbody\n"
        ),
    )
    .unwrap();
}

#[test]
fn handoff_guards_missing_task_review_and_clean_report() {
    let dir = TmpDir::new("handoff-guards");
    let mut empty = app_with(&dir, &[]);
    empty.handoff_selected();
    assert_eq!(empty.status_msg, "no task selected");

    let dir2 = TmpDir::new("handoff-noreview");
    let mut app = app_with(&dir2, &[("TASK-001", "review")]);
    app.handoff_selected();
    assert!(app.status_msg.contains("run review"));

    fs::write(
        dir2.path().join(".backlog/TASK-001.review.md"),
        "---\ntask: TASK-001\nfindings: []\nconstraints: []\n---\nok\n",
    )
    .unwrap();
    app.handoff_selected();
    assert!(app.status_msg.contains("clean review"));
}

#[test]
fn handoff_sends_findings_to_existing_play_session() {
    let dir = TmpDir::new("handoff-live");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    write_dirty_review(&dir, "TASK-001");
    app.refresh().unwrap();
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u-h",
        dir.path(),
    );
    app.handoff_selected();
    assert!(
        app.status_msg.contains("findings sent"),
        "{}",
        app.status_msg
    );
    assert_eq!(app.board_focus, BoardFocus::Chat);
    app.stop_all_sessions();
}

#[test]
fn handoff_opens_play_when_none_is_live() {
    let dir = TmpDir::new("handoff-open");
    let repo = git_repo(dir.path());
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "review", "feat/handoff-work");
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo,
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.executor = ClaudeExecutor::with_bin("cat");
    app.gh = Gh::with_bin("false");
    write_dirty_review(&dir, "TASK-001");
    app.refresh().unwrap();

    app.handoff_selected();
    assert!(
        app.status_msg.contains("findings sent"),
        "{}",
        app.status_msg
    );
    assert_eq!(app.sessions.len(), 1);
    app.close_selected_session();
}

#[test]
fn handoff_gives_up_when_play_cannot_start() {
    let dir = TmpDir::new("handoff-fail");
    // no repo mapped: the implicit play_selected fails and handoff returns
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    app.executor = ClaudeExecutor::with_bin("cat");
    write_dirty_review(&dir, "TASK-001");
    app.refresh().unwrap();
    app.handoff_selected();
    assert!(app.status_msg.contains("play failed"), "{}", app.status_msg);
    assert!(app.sessions.is_empty());
}

#[test]
fn finish_selected_reports_merge_state_and_errors() {
    let dir = TmpDir::new("finish");
    let mut empty = app_with(&dir, &[]);
    empty.finish_selected();
    assert_eq!(empty.status_msg, HINT);

    let dir2 = TmpDir::new("finish-ok");
    // repos unmapped: gh is never called, state aggregates to NotCreated
    let mut app = app_with(&dir2, &[("TASK-001", "wip")]);
    app.finish_selected();
    assert!(
        app.status_msg.contains("finish TASK-001"),
        "{}",
        app.status_msg
    );

    fs::remove_file(dir2.path().join(".backlog/TASK-001.md")).unwrap();
    app.finish_selected();
    assert!(
        app.status_msg.contains("finish failed"),
        "{}",
        app.status_msg
    );
}

// --- async jobs -------------------------------------------------------------

#[test]
fn job_running_and_guards_against_concurrent_jobs() {
    let dir = TmpDir::new("job-guard");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(!app.job_running());
    let (_tx, job) = fake_job(JobKind::Ingest);
    app.job = Some(job);
    assert!(app.job_running());

    app.start_ingest_job();
    app.start_review_job_for("TASK-001", Vec::new());
    app.start_parallel_job();
    app.start_capture_job("hint");
    app.start_init_job("/tmp");
    assert_eq!(app.job.as_ref().unwrap().title, "fake");
}

#[test]
fn ingest_job_creates_stub_and_chains_parallel_analysis() {
    let dir = TmpDir::new("job-ingest");
    let mut app = app_with(&dir, &[]);
    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"tasks":[{"title":"t","objetivo":"do x"}],"docs":[]}"#,
    );
    app.start_ingest_job();
    assert!(app.job_overlay);
    wait_job(&mut app);
    // one open task after ingest: the chained analysis ends without conflicts
    assert!(app.status_msg.contains("parallelism"), "{}", app.status_msg);
    assert_eq!(app.tasks.len(), 1);
    assert!(dir.path().join(".jaum/parallel.json").exists());
    app.dismiss_job();
    assert!(app.job.is_none());
    assert!(!app.job_overlay);
}

#[test]
fn ingest_job_reports_failure() {
    let dir = TmpDir::new("job-ingest-err");
    let mut app = app_with(&dir, &[]);
    app.claude_bin = "false".into();
    app.start_ingest_job();
    wait_job(&mut app);
    assert!(
        app.status_msg.contains("ingest failed"),
        "{}",
        app.status_msg
    );
    let logs = &app.job.as_ref().unwrap().logs;
    assert!(logs.iter().any(|l| l.contains("ingest failed")));
}

#[test]
fn review_job_writes_report_without_opening_the_overlay() {
    let dir = TmpDir::new("job-review-ok");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"findings":[{"file":"src/x.rs","message":"bug","severity":"major"}],"constraints":[],"criteria":[]}"#,
    );
    app.start_review_job_for("TASK-001", vec![("org/x".to_string(), "abc".to_string())]);
    assert!(
        !app.job_overlay,
        "auto-dispatched review must not steal the screen"
    );
    assert!(
        app.status_msg.contains("review started"),
        "{}",
        app.status_msg
    );
    wait_job(&mut app);
    assert!(
        app.status_msg.contains("review TASK-001: 1 finding(s)"),
        "{}",
        app.status_msg
    );
    assert!(dir.path().join(".backlog/TASK-001.review.md").exists());
    // the reviewed SHA is stamped only after a successful capture
    assert_eq!(
        app.store.get("TASK-001").unwrap().prs[0]
            .reviewed_sha
            .as_deref(),
        Some("abc")
    );
}

#[test]
fn review_job_failure_leaves_the_sha_unmarked_for_retry() {
    let dir = TmpDir::new("job-review-err");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    app.claude_bin = "false".into();
    app.start_review_job_for("TASK-001", vec![("org/x".to_string(), "abc".to_string())]);
    wait_job(&mut app);
    assert!(
        app.status_msg.contains("review failed"),
        "{}",
        app.status_msg
    );
    // a failed capture must NOT stamp the SHA: the next poll has to retry it
    assert_eq!(app.store.get("TASK-001").unwrap().prs[0].reviewed_sha, None);
}

#[test]
fn reviewing_task_id_tracks_the_running_capture_and_statusline() {
    let dir = TmpDir::new("reviewing-id");
    let mut app = app_with(&dir, &[("TASK-001", "review")]);
    assert_eq!(app.reviewing_task_id(), None);

    // a non-review job in flight does not count as a running review
    let (_tx, mut job) = fake_job(JobKind::Ingest);
    job.task = Some("TASK-001".into());
    app.job = Some(job);
    assert_eq!(app.reviewing_task_id(), None);

    // a running review job exposes its task and shows in the statusline
    let (_tx2, mut rjob) = fake_job(JobKind::Review);
    rjob.task = Some("TASK-001".into());
    app.job = Some(rjob);
    app.selected = 0;
    assert_eq!(app.reviewing_task_id(), Some("TASK-001"));
    assert!(
        app.statusline().contains("⟳ review TASK-001"),
        "{}",
        app.statusline()
    );

    // a finished review no longer counts
    app.job.as_mut().unwrap().finished = true;
    assert_eq!(app.reviewing_task_id(), None);
}

#[test]
fn parallel_job_persists_conflicts() {
    let dir = TmpDir::new("job-parallel");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"conflicts":[{"a":"TASK-001","b":"TASK-002","repo":"org/x","reason":"same file"}]}"#,
    );
    app.start_parallel_job();
    wait_job(&mut app);
    assert!(
        app.status_msg.contains("parallelism: 1 conflict(s)"),
        "{}",
        app.status_msg
    );
    assert!(app.parallel.is_some());
}

#[test]
fn parallel_job_reports_failure() {
    let dir = TmpDir::new("job-parallel-err");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    app.claude_bin = "false".into();
    app.start_parallel_job();
    wait_job(&mut app);
    assert!(
        app.status_msg.contains("parallelism analysis failed"),
        "{}",
        app.status_msg
    );
}

#[test]
fn capture_job_creates_tasks_from_hint() {
    let dir = TmpDir::new("job-capture");
    let mut app = app_with(&dir, &[]);
    app.start_capture_job("   ");
    assert!(app.status_msg.contains("capture cancelled"));

    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"tasks":[{"title":"t","objetivo":"fix the parser"}],"docs":[]}"#,
    );
    app.start_capture_job("parser breaks on empty input");
    wait_job(&mut app);
    // capture chains nothing; claude's stub created exactly one task
    assert!(
        app.status_msg.contains("claude created: TASK-"),
        "{}",
        app.status_msg
    );

    let dir2 = TmpDir::new("job-capture-err");
    let mut bad = app_with(&dir2, &[]);
    bad.claude_bin = "false".into();
    bad.start_capture_job("anything");
    wait_job(&mut bad);
    assert!(
        bad.status_msg.contains("capture failed"),
        "{}",
        bad.status_msg
    );
}

fn init_ok_with_repo(root: &Path, _explicit: &[PathBuf]) -> Result<Project> {
    Ok(Project {
        name: "proj-init".into(),
        root: root.to_path_buf(),
        backlog: root.join("backlog"),
        docs: root.join("docs"),
        work_dir: root.join("work"),
        repos: vec![RepoMap {
            slug: "o/r".into(),
            path: root.join("r"),
        }],
    })
}

fn init_ok_no_repos(root: &Path, _explicit: &[PathBuf]) -> Result<Project> {
    Ok(Project {
        name: "proj-init".into(),
        root: root.to_path_buf(),
        backlog: root.join("backlog"),
        docs: root.join("docs"),
        work_dir: root.join("work"),
        repos: Vec::new(),
    })
}

fn load_config_empty() -> Result<GlobalConfig> {
    Ok(GlobalConfig {
        ci_poll_secs: None,
        projects: Vec::new(),
    })
}

fn load_config_failing() -> Result<GlobalConfig> {
    anyhow::bail!("no config in tests")
}

fn init_target_dir() -> PathBuf {
    std::env::temp_dir().join(format!("jaum-app-unit-initcfg-{}", std::process::id()))
}

fn load_config_with_init_project() -> Result<GlobalConfig> {
    let base = init_target_dir();
    fs::create_dir_all(base.join(".backlog"))?;
    fs::create_dir_all(base.join("docs"))?;
    Ok(GlobalConfig {
        ci_poll_secs: None,
        projects: vec![Project {
            name: "proj-init".into(),
            root: base.clone(),
            backlog: base.join(".backlog"),
            docs: base.join("docs"),
            work_dir: base.join(".jaum"),
            repos: Vec::new(),
        }],
    })
}

#[test]
fn init_job_validates_input_and_reports_failure() {
    let dir = TmpDir::new("job-init-err");
    let mut app = app_with(&dir, &[]);
    app.start_init_job("  ");
    assert!(app.status_msg.contains("init cancelled"));

    app.start_init_job("/definitely/missing/jaum-test-path");
    wait_job(&mut app);
    assert!(app.status_msg.contains("init failed"), "{}", app.status_msg);
}

#[test]
fn init_job_success_registers_and_loads_the_project() {
    let dir = TmpDir::new("job-init-ok");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.init_project = init_ok_no_repos;
    app.load_config = load_config_with_init_project;
    app.start_init_job(&dir.path().to_string_lossy());
    wait_job(&mut app);
    // switched to the freshly registered project, which still needs setup
    assert_eq!(app.project_name(), "proj-init");
    assert!(
        app.status_msg
            .contains("'proj-init' registered — setup pending"),
        "{}",
        app.status_msg
    );
    let _ = fs::remove_dir_all(init_target_dir());
}

#[test]
fn init_job_success_without_matching_config_entry() {
    let dir = TmpDir::new("job-init-plain");
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/clean-branch");
    fs::write(dir.path().join("conventions.md"), "- always test\n").unwrap();
    fs::write(dir.path().join("setup.md"), "map\n").unwrap();
    let mut app = app_with(&dir, &[]);
    app.init_project = init_ok_with_repo;
    app.load_config = load_config_failing;
    app.start_init_job(&dir.path().to_string_lossy());
    wait_job(&mut app);
    // config load failed: stays on the (fully set up) current project
    assert_eq!(app.project_name(), "test");
    assert_eq!(app.status_msg, "project 'proj-init' registered");
    let logs = &app.job.as_ref().unwrap().logs;
    assert!(logs.iter().any(|l| l.contains("repo o/r ->")), "{logs:?}");
    assert!(logs.iter().any(|l| l.contains("project 'proj-init' ready")));
}

#[test]
fn init_job_logs_when_no_repos_detected() {
    let dir = TmpDir::new("job-init-norepo");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.init_project = init_ok_no_repos;
    app.load_config = load_config_empty;
    app.start_init_job(&dir.path().to_string_lossy());
    wait_job(&mut app);
    let logs = &app.job.as_ref().unwrap().logs;
    assert!(
        logs.iter().any(|l| l.contains("no repos detected")),
        "{logs:?}"
    );
}

#[test]
fn poll_job_accumulates_logs_and_is_noop_without_job() {
    let dir = TmpDir::new("poll");
    let mut app = app_with(&dir, &[]);
    app.poll_job(); // no job: nothing to do

    let (tx, job) = fake_job(JobKind::Capture);
    app.job = Some(job);
    tx.send(JobMsg::Log("working".into())).unwrap();
    app.poll_job();
    assert_eq!(app.job.as_ref().unwrap().logs, vec!["working".to_string()]);
    assert!(!app.job.as_ref().unwrap().finished);

    tx.send(JobMsg::Done(Ok("all done".into()))).unwrap();
    app.poll_job();
    assert!(app.job.as_ref().unwrap().finished);
    assert_eq!(app.status_msg, "all done");
}

#[test]
fn dismiss_job_keeps_running_jobs_in_background() {
    let dir = TmpDir::new("dismiss");
    let mut app = app_with(&dir, &[]);
    let (_tx, job) = fake_job(JobKind::Ingest);
    app.job = Some(job);
    app.job_overlay = true;
    app.dismiss_job();
    assert!(!app.job_overlay);
    assert!(app.job.is_some());

    app.job.as_mut().unwrap().finished = true;
    app.dismiss_job();
    assert!(app.job.is_none());
}

#[test]
fn job_log_scrolling_toggles_follow() {
    let dir = TmpDir::new("job-scroll");
    let mut app = app_with(&dir, &[]);
    // without a job nothing panics
    app.job_scroll_up();
    app.job_scroll_down();
    app.job_scroll_top();
    app.job_follow();

    let (_tx, job) = fake_job(JobKind::Ingest);
    app.job = Some(job);
    app.job.as_mut().unwrap().scroll = 5;
    app.job_scroll_up();
    let j = app.job.as_ref().unwrap();
    assert!(!j.follow);
    assert_eq!(j.scroll, 4);
    app.job_scroll_down();
    assert_eq!(app.job.as_ref().unwrap().scroll, 5);
    app.job_scroll_top();
    assert_eq!(app.job.as_ref().unwrap().scroll, 0);
    app.job_follow();
    assert!(app.job.as_ref().unwrap().follow);
}

// --- defer / conventions / quick tasks --------------------------------------

#[test]
fn defer_validates_selection_text_and_store_errors() {
    let dir = TmpDir::new("defer");
    let mut empty = app_with(&dir, &[]);
    empty.defer("text"); // no task selected: silently ignored
    assert_eq!(empty.status_msg, HINT);

    let dir2 = TmpDir::new("defer-ok");
    let mut app = app_with(&dir2, &[("TASK-001", "wip")]);
    app.defer("  ");
    assert!(app.status_msg.contains("defer cancelled"));
    app.defer("extract the parser");
    assert!(app.status_msg.contains("deferred from TASK-001"));
    assert_eq!(app.tasks.len(), 2);

    fs::remove_file(dir2.path().join(".backlog/TASK-001.md")).unwrap();
    app.selected = app.tasks.iter().position(|t| t.id == "TASK-001").unwrap();
    app.defer("again");
    assert!(
        app.status_msg.contains("defer failed"),
        "{}",
        app.status_msg
    );
}

#[test]
fn add_convention_appends_and_reports_write_errors() {
    let dir = TmpDir::new("conv");
    let mut app = app_with(&dir, &[]);
    app.add_convention("   ");
    assert!(app.status_msg.contains("convention cancelled"));

    // existing content without a trailing newline gets one before the bullet
    fs::write(&app.conventions_path, "# heading").unwrap();
    app.add_convention("no emojis anywhere");
    assert_eq!(app.conventions, "# heading\n- no emojis anywhere\n");
    assert_eq!(app.status_msg, "convention added");

    app.conventions_path = dir.path().to_path_buf(); // a directory: write fails
    app.add_convention("does not matter");
    assert!(app.status_msg.contains("failed to write convention"));
}

#[test]
fn new_task_quick_creates_and_reports_errors() {
    let dir = TmpDir::new("quick");
    let mut app = app_with(&dir, &[]);
    app.new_task_quick("  ");
    assert!(app.status_msg.contains("task cancelled"));

    app.new_task_quick("write the parser");
    assert!(app.status_msg.contains("task created"));
    assert_eq!(app.tasks.len(), 1);
    assert!(app.tasks[0].body.contains("write the parser"));

    // a store rooted at a file cannot create tasks
    let file = dir.path().join("not-a-dir");
    fs::write(&file, "x").unwrap();
    app.store = Store::new(&file);
    app.new_task_quick("boom");
    assert!(
        app.status_msg.contains("failed to create task"),
        "{}",
        app.status_msg
    );
}

// --- input capture -----------------------------------------------------------

#[test]
fn input_capture_dispatches_by_kind() {
    let dir = TmpDir::new("input");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.start_input(InputKind::Defer);
    assert!(matches!(app.input, Some((InputKind::Defer, ref s)) if s.is_empty()));

    app.start_init_input();
    let (kind, buf) = app.input.clone().unwrap();
    assert!(kind == InputKind::InitPath);
    assert!(!buf.is_empty());

    app.submit_input(InputKind::Defer, "split the migration".into());
    assert!(app.status_msg.contains("deferred"));
    app.submit_input(InputKind::Convention, "prefer small functions".into());
    assert!(app.status_msg.contains("convention added"));
    app.submit_input(InputKind::NewTask, "quick one".into());
    assert!(app.status_msg.contains("task created"));
    app.submit_input(InputKind::NewTaskClaude, "  ".into());
    assert!(app.status_msg.contains("capture cancelled"));
    app.submit_input(InputKind::InitPath, "  ".into());
    assert!(app.status_msg.contains("init cancelled"));
}

// --- reload, pr sync, toast ---------------------------------------------------

#[test]
fn tick_reload_reacts_to_watcher_events_and_time_fallback() {
    let dir = TmpDir::new("reload");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);

    // no event and a recent reload: nothing happens
    app.watch_rx = None;
    app.last_reload = Instant::now();
    write_task(&dir.path().join(".backlog"), "TASK-002", "wip", "feat/two");
    app.tick_reload();
    assert_eq!(app.tasks.len(), 1);

    // an injected watcher event forces an immediate reload
    let (tx, rx) = channel();
    app.watch_rx = Some(rx);
    tx.send(()).unwrap();
    app.tick_reload();
    assert_eq!(app.tasks.len(), 2);

    // and the ~2s fallback reloads even without events
    app.watch_rx = None;
    write_task(
        &dir.path().join(".backlog"),
        "TASK-003",
        "wip",
        "feat/three",
    );
    app.last_reload = past(3);
    app.tick_reload();
    assert_eq!(app.tasks.len(), 3);
}

#[test]
fn pr_sync_targets_need_live_play_and_unset_pr() {
    let dir = TmpDir::new("targets");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(app.pr_sync_targets().is_empty());

    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    assert_eq!(
        app.pr_sync_targets(),
        vec![(
            "TASK-001".to_string(),
            "org/x".to_string(),
            "feat/task-001".to_string()
        )]
    );
    // sessions with no task, or whose task left the board, contribute nothing
    open_cat(&mut app, SessionKind::Play, None, "u2", dir.path());
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-404"),
        "u3",
        dir.path(),
    );
    assert_eq!(app.pr_sync_targets().len(), 1);

    for e in &mut app.sessions {
        e.finished = true;
    }
    assert!(app.pr_sync_targets().is_empty());
    app.stop_all_sessions();
}

#[test]
fn tick_pr_sync_throttles_and_persists_discovered_numbers() {
    let dir = TmpDir::new("prsync");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "wip", "feat/sync-me");
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo_dir,
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.gh_bin = write_script(dir.path(), "gh-stub", "#!/bin/sh\necho 7\n");

    // while a pass is flagged as running, nothing starts
    app.pr_sync_running.store(true, Ordering::Relaxed);
    let before = app.last_pr_sync;
    app.tick_pr_sync();
    assert!(app.last_pr_sync == before);
    app.pr_sync_running.store(false, Ordering::Relaxed);

    // throttled while the last pass is recent
    app.last_pr_sync = Instant::now();
    app.tick_pr_sync();
    assert_eq!(app.store.get("TASK-001").unwrap().prs[0].pr, 0);

    // no live play session: resets the clock and does nothing
    app.last_pr_sync = past(21);
    app.tick_pr_sync();
    assert!(app.last_pr_sync > past(2));

    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    app.last_pr_sync = past(21);
    app.tick_pr_sync();
    wait_until(|| {
        app.store
            .get("TASK-001")
            .map(|t| t.prs[0].pr == 7)
            .unwrap_or(false)
    });
    wait_until(|| !app.pr_sync_running.load(Ordering::Relaxed));
    app.stop_all_sessions();
}

// --- CI watch (auto review on green checks) ----------------------------------

/// Task markdown with an explicit PR number (and optional reviewed marker).
fn write_task_with_pr(backlog: &Path, id: &str, status: &str, pr: u64, reviewed: Option<&str>) {
    let marker = reviewed
        .map(|s| format!("\n    reviewed_sha: {s}"))
        .unwrap_or_default();
    fs::create_dir_all(backlog).unwrap();
    fs::write(
        backlog.join(format!("{id}.md")),
        format!(
            "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: org/x\n    pr: {pr}\n    branch: feat/x{marker}\n---\n\n## Objective\nx\n"
        ),
    )
    .unwrap();
}

fn green(sha: &str) -> Vec<(String, PrCi)> {
    vec![(
        "org/x".to_string(),
        PrCi {
            state: MergeState::Open,
            checks: CiStatus::Passing,
            head_sha: sha.to_string(),
        },
    )]
}

#[test]
fn ci_watch_targets_need_created_prs_on_open_impl_tasks() {
    let dir = TmpDir::new("ci-targets");
    let backlog = dir.path().join(".backlog");
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    write_task_with_pr(&backlog, "TASK-002", "wip", 0, None); // PR not created
    write_task_with_pr(&backlog, "TASK-003", "merged", 9, None); // already merged
    write_unlinked_task(&backlog, "TASK-004", "backlog"); // no PR links
    fs::write(
        backlog.join("TASK-005.md"),
        "---\nid: TASK-005\ntype: spike\nstatus: wip\nprs:\n  - repo: org/x\n    pr: 8\n    branch: feat/s\n---\n\n## Objective\nx\n",
    )
    .unwrap();

    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir.path(), Vec::new())],
    };
    let app = App::new(cfg, 0).unwrap();
    assert_eq!(
        app.ci_watch_targets(),
        vec![("TASK-001".to_string(), vec![("org/x".to_string(), 7)])]
    );
}

#[test]
fn ci_green_observation_marks_sha_and_starts_review_once() {
    let dir = TmpDir::new("ci-trigger");
    let backlog = dir.path().join(".backlog");
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir.path(), Vec::new())],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"findings":[],"constraints":[],"criteria":[]}"#,
    );

    // green CI on a fresh SHA: review job dispatched; the SHA is stamped only
    // once the capture succeeds (background thread).
    app.ci_tx.send(("TASK-001".into(), green("abc"))).unwrap();
    app.tick_ci_watch();
    assert!(app.job_running(), "review job should have started");
    assert_eq!(app.job.as_ref().unwrap().title, "review TASK-001");
    wait_job(&mut app);
    assert!(backlog.join("TASK-001.review.md").exists());
    assert_eq!(
        app.store.get("TASK-001").unwrap().prs[0]
            .reviewed_sha
            .as_deref(),
        Some("abc")
    );

    // same SHA green again: idempotent, no new job
    app.job = None;
    app.ci_tx.send(("TASK-001".into(), green("abc"))).unwrap();
    app.tick_ci_watch();
    assert!(app.job.is_none(), "same commit must not re-trigger");

    // a new push that turns green re-arms the trigger
    app.ci_tx.send(("TASK-001".into(), green("def"))).unwrap();
    app.tick_ci_watch();
    assert!(app.job_running(), "new commit should re-trigger");
    wait_job(&mut app);
    assert_eq!(
        app.store.get("TASK-001").unwrap().prs[0]
            .reviewed_sha
            .as_deref(),
        Some("def")
    );
}

#[test]
fn ci_observations_that_are_not_fully_green_never_trigger() {
    let dir = TmpDir::new("ci-notgreen");
    let backlog = dir.path().join(".backlog");
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir.path(), Vec::new())],
    };
    let mut app = App::new(cfg, 0).unwrap();

    for checks in [
        CiStatus::Pending,
        CiStatus::Failing,
        CiStatus::NoChecks,
        CiStatus::Unknown,
    ] {
        let observed = vec![(
            "org/x".to_string(),
            PrCi {
                state: MergeState::Open,
                checks,
                head_sha: "abc".into(),
            },
        )];
        app.ci_tx.send(("TASK-001".into(), observed)).unwrap();
    }
    // a task that vanished from the backlog is skipped gracefully
    app.ci_tx.send(("TASK-404".into(), green("abc"))).unwrap();
    app.tick_ci_watch();
    assert!(app.job.is_none(), "nothing should trigger");
    assert_eq!(app.store.get("TASK-001").unwrap().prs[0].reviewed_sha, None);
}

#[test]
fn ci_trigger_defers_while_another_job_runs() {
    let dir = TmpDir::new("ci-defer");
    let backlog = dir.path().join(".backlog");
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(dir.path(), Vec::new())],
    };
    let mut app = App::new(cfg, 0).unwrap();

    let (_tx, job) = fake_job(JobKind::Ingest);
    app.job = Some(job);
    app.ci_tx.send(("TASK-001".into(), green("abc"))).unwrap();
    app.tick_ci_watch();
    // dropped without marking: the next green poll re-arms the trigger
    assert_eq!(app.store.get("TASK-001").unwrap().prs[0].reviewed_sha, None);
    assert_eq!(app.job.as_ref().unwrap().title, "fake");
}

#[test]
fn tick_ci_watch_polls_gh_and_dispatches_the_review() {
    let dir = TmpDir::new("ci-poll");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    let backlog = dir.path().join(".backlog");
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    let cfg = GlobalConfig {
        ci_poll_secs: Some(120),
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo_dir,
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    assert_eq!(app.ci_poll_interval, Duration::from_secs(120));
    app.gh_bin = write_script(
        dir.path(),
        "gh-ci-stub",
        "#!/bin/sh\necho '{\"state\":\"OPEN\",\"headRefOid\":\"abc\",\"statusCheckRollup\":[{\"status\":\"COMPLETED\",\"conclusion\":\"SUCCESS\"}]}'\n",
    );
    app.claude_bin = stub_claude(
        dir.path(),
        r#"{"findings":[],"constraints":[],"criteria":[]}"#,
    );

    // while a pass is flagged as running, nothing starts
    app.ci_poll_running.store(true, Ordering::Relaxed);
    let before = app.last_ci_poll;
    app.tick_ci_watch();
    assert!(app.last_ci_poll == before);
    app.ci_poll_running.store(false, Ordering::Relaxed);

    // throttled while the last pass is recent
    app.last_ci_poll = Instant::now();
    app.tick_ci_watch();
    assert!(!app.ci_poll_running.load(Ordering::Relaxed));

    // due: polls gh in background, then the next tick applies the observation
    // and dispatches the review; the SHA is stamped once the capture succeeds.
    app.last_ci_poll = past(121);
    app.tick_ci_watch();
    wait_until(|| !app.ci_poll_running.load(Ordering::Relaxed));
    app.tick_ci_watch();
    wait_job(&mut app);
    assert!(backlog.join("TASK-001.review.md").exists());
    assert_eq!(
        app.store.get("TASK-001").unwrap().prs[0]
            .reviewed_sha
            .as_deref(),
        Some("abc")
    );
}

#[test]
fn tick_ci_watch_gh_failure_is_unknown_and_never_triggers() {
    let dir = TmpDir::new("ci-poll-err");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    let backlog = dir.path().join(".backlog");
    write_task_with_pr(&backlog, "TASK-001", "wip", 7, None);
    // a second PR whose repo is not mapped locally also degrades to Unknown
    fs::write(
        backlog.join("TASK-002.md"),
        "---\nid: TASK-002\ntype: impl\nstatus: wip\nprs:\n  - repo: org/unmapped\n    pr: 9\n    branch: feat/y\n---\n\n## Objective\nx\n",
    )
    .unwrap();
    let cfg = GlobalConfig {
        ci_poll_secs: None,
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo_dir,
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.gh_bin = "false".into();

    app.last_ci_poll = past(31);
    app.tick_ci_watch();
    wait_until(|| !app.ci_poll_running.load(Ordering::Relaxed));
    app.tick_ci_watch();
    assert!(app.job.is_none(), "gh failure must not trigger a review");
    assert_eq!(app.store.get("TASK-001").unwrap().prs[0].reviewed_sha, None);
    assert_eq!(app.store.get("TASK-002").unwrap().prs[0].reviewed_sha, None);
}

#[test]
fn toast_tracks_status_changes_and_expires() {
    let dir = TmpDir::new("toast");
    let mut app = app_with(&dir, &[]);
    assert!(app.active_toast().is_none());

    app.status_msg = "something happened".into();
    app.tick_toast();
    assert_eq!(app.active_toast(), Some("something happened"));
    // unchanged message does not restart the toast
    let started = app.toast_started;
    app.tick_toast();
    assert_eq!(app.toast_started, started);

    app.toast_started = Some(past(4));
    assert!(app.active_toast().is_none());

    app.rearm_toast();
    assert_eq!(app.active_toast(), Some("something happened"));
}

// --- sessions lifecycle -------------------------------------------------------

#[test]
fn open_session_focuses_owner_task_and_persists() {
    let dir = TmpDir::new("open-focus");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    app.selected = 1;
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    assert_eq!(app.tab, Tab::Board);
    assert!(!app.project_selected);
    assert_eq!(app.selected_task().unwrap().id, "TASK-001");
    assert_eq!(app.board_focus, BoardFocus::Chat);
    assert!(dir.path().join(".jaum/sessions.json").exists());
    let recs = app.load_session_records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].claude_session_id, "u1");
    app.stop_all_sessions();
}

#[test]
fn focus_session_handles_unknown_task_and_history_cards() {
    let dir = TmpDir::new("focus-edge");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-999"),
        "u-x",
        dir.path(),
    )));
    app.selected = 0;
    app.focus_session(0);
    // unknown task: selection stays, focus lands on the (dead) cards column
    assert_eq!(app.selected, 0);
    assert_eq!(app.board_focus, BoardFocus::Cards);
}

#[test]
fn finish_and_close_session_require_a_selected_card() {
    let dir = TmpDir::new("no-card");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.finish_selected_session(); // no cards: both are no-ops
    app.close_selected_session();
    assert!(app.sessions.is_empty());
}

#[test]
fn finish_session_keeps_history_close_removes_it() {
    let dir = TmpDir::new("fin-close");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    );
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u2",
        dir.path(),
    );

    app.finish_selected_session();
    assert_eq!(app.sessions.len(), 2);
    assert!(app.status_msg.contains("session finished"));
    let cur = app.current_session_idx().unwrap();
    assert!(!app.sessions[cur].is_live());

    app.board_focus = BoardFocus::Chat;
    app.close_selected_session();
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.board_focus, BoardFocus::Cards);
    app.stop_all_sessions();
    assert!(app.sessions.is_empty());
    assert_eq!(app.card_selected, 0);
}

#[test]
fn cleanup_worktrees_ignores_sessions_without_task() {
    let dir = TmpDir::new("cleanup-none");
    let app = app_with(&dir, &[("TASK-001", "wip")]);
    // must not touch git at all
    app.cleanup_worktrees(&None, &[("org/x".into(), dir.path().to_path_buf())]);
    app.cleanup_worktrees(
        &Some("TASK-404".into()),
        &[("org/x".into(), dir.path().to_path_buf())],
    );
}
