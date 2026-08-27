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

/// Points the app's sidecar at the scripted node stub (no real claude).
fn sidecar_stub(app: &mut App) {
    app.node_bin = "node".into();
    app.sidecar_bundle = crate::sidecar::sidecar_tests::stub().to_path_buf();
}

/// Registers a sidecar-backed play entry without sending any turn.
fn open_sidecar_play(app: &mut App, task: &str, session_id: &str, cwd: &Path) -> usize {
    let log = SessionLog::new(&app.home(), session_id);
    app.sessions.push(SessionEntry::sidecar(
        SessionKind::Play,
        app.project_name().to_string(),
        Some(task.into()),
        Vec::new(),
        session_id.into(),
        cwd.to_path_buf(),
        log,
    ));
    app.sessions.len() - 1
}

/// Pumps sidecar events and permission deadlines until `pred` holds (10s cap).
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
        project: "test".into(),
        task: task.map(str::to_string),
        claude_session_id: uuid.into(),
        cwd: cwd.to_path_buf(),
        worktrees: Vec::new(),
        created_ms: 1_700_000_000_000,
        last_activity_ms: 1_700_000_001_000,
        finished: false,
        blocked: false,
    }
}

fn fake_job(kind: JobKind) -> (std::sync::mpsc::Sender<JobMsg>, Job) {
    let (tx, rx) = channel();
    (
        tx,
        Job {
            kind,
            title: "fake".into(),
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
    assert_eq!(Tab::Board.index(), 0);
    assert_eq!(Tab::from_index(9), Tab::Docs);
    assert_eq!(Tab::Board.next(), Tab::Docs);
    assert_eq!(Tab::Docs.next(), Tab::Board);
}

#[test]
fn status_labels_and_board_order() {
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
    let rec = record(SessionKind::Play, Some("TASK-001"), "u-rec", dir.path());
    let entry = SessionEntry::history(&rec);
    assert!(!entry.is_live());
    assert!(entry.finished);
    assert_eq!(entry.name(), "play · TASK-001");
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
    let (a, b, repo) = app.overlaps.first().expect("overlap detected");
    assert_eq!(
        (a.as_str(), b.as_str(), repo.as_str()),
        ("TASK-001", "TASK-002", "org/x")
    );
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

    let setup = app.rehydrate_one(record(SessionKind::Setup, None, "u-s", dir.path()));
    assert!(setup.is_live());

    for mut e in [play, setup] {
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
    // a play record without a task cannot resume
    let p = app.rehydrate_one(record(SessionKind::Play, None, "u-p", dir.path()));
    assert!(!p.is_live());
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
fn load_project_switches_but_keeps_other_projects_sessions_running() {
    let dir1 = TmpDir::new("proj-a");
    let dir2 = TmpDir::new("proj-b");
    let backlog2 = dir2.path().join(".backlog");
    write_task(&backlog2, "TASK-009", "wip", "feat/other");
    let mut p2 = project(dir2.path(), Vec::new());
    p2.name = "second".into();
    let cfg = GlobalConfig {
        projects: vec![project(dir1.path(), Vec::new()), p2],
    };
    fs::create_dir_all(dir1.path().join(".backlog")).unwrap();
    let mut app = App::new(cfg, 0).unwrap();
    open_cat(&mut app, SessionKind::Setup, None, "u-set", dir1.path());
    assert_eq!(app.sessions.len(), 1);

    app.load_project(1);
    assert_eq!(app.current, 1);
    assert_eq!(app.project_name(), "second");
    // the "test" project's setup session is NOT killed: it keeps running in the
    // background instead of disappearing from the daemon's session list.
    assert_eq!(app.sessions.len(), 1);
    assert!(app.sessions[0].is_live(), "background session stays live");
    assert_eq!(app.sessions[0].project, "test");
    // but it must not leak onto "second"'s board: no cards belong to it here
    // (the · project row is still selected from the setup session's own focus).
    assert!(app.project_selected);
    assert!(
        app.task_cards().is_empty(),
        "another project's setup session must not show up on this board"
    );
    assert_eq!(app.tasks.len(), 1);
    assert!(app.status_msg.contains("second"));

    // switching back finds the same live session again (no duplicate from rehydration)
    app.load_project(0);
    assert_eq!(app.sessions.len(), 1);
    assert!(app.sessions[0].is_live());

    // out-of-range index is ignored
    app.load_project(7);
    assert_eq!(app.current, 0);

    app.stop_all_sessions();
}

#[test]
fn same_task_id_in_two_projects_does_not_collide_on_the_board() {
    let dir1 = TmpDir::new("collide-a");
    let dir2 = TmpDir::new("collide-b");
    // both projects have a task with the SAME id: ids are only unique within
    // one project's own `.backlog/`.
    write_task(&dir1.path().join(".backlog"), "TASK-001", "wip", "feat/a");
    write_task(
        &dir2.path().join(".backlog"),
        "TASK-001",
        "backlog",
        "feat/b",
    );
    let mut p2 = project(dir2.path(), Vec::new());
    p2.name = "second".into();
    let cfg = GlobalConfig {
        projects: vec![project(dir1.path(), Vec::new()), p2],
    };
    let mut app = App::new(cfg, 0).unwrap();
    // a live play session for "test"'s TASK-001
    open_cat(
        &mut app,
        SessionKind::Play,
        Some("TASK-001"),
        "u-a",
        dir1.path(),
    );
    assert!(app.sessions.iter().any(|e| e.is_live()));

    app.load_project(1);
    assert_eq!(app.project_name(), "second");
    app.selected = app.tasks.iter().position(|t| t.id == "TASK-001").unwrap();
    app.card_selected = 0;

    // "test"'s live session for its own TASK-001 must not surface here: no card.
    assert!(app.task_cards().is_empty());
    assert!(!app.selected_card_is_live());

    app.stop_all_sessions();
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
fn task_cards_lists_sessions() {
    let dir = TmpDir::new("cards");
    let mut app = app_with(&dir, &[("TASK-001", "review"), ("TASK-002", "review")]);
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Play,
        Some("TASK-001"),
        "u1",
        dir.path(),
    )));
    app.selected = app.tasks.iter().position(|t| t.id == "TASK-001").unwrap();
    let cards = app.task_cards();
    assert_eq!(cards, vec![BoardCard::Session(0)]);

    app.card_selected = 0;
    assert_eq!(app.selected_card(), Some(BoardCard::Session(0)));
    assert!(app.current_session_idx().is_some());
    assert!(!app.selected_card_is_live());

    // card cursor bounds (single card: stays put)
    app.card_next();
    assert_eq!(app.card_selected, 0);
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

// --- play / finish ----------------------------------------------------------

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
    assert!(app.sessions[0].session.is_some());
    assert!(app.sessions[0].is_live());
    let wt = repo.with_file_name("repo.worktrees").join("feat-nice-work");
    assert!(wt.exists());
    assert_eq!(app.tasks[0].status, Status::Wip);

    app.close_selected_session();
    assert!(app.sessions.is_empty());
    assert!(!wt.exists());
}

#[test]
fn play_selected_resumes_a_finished_sessions_claude_id() {
    let dir = TmpDir::new("play-resume");
    let repo = git_repo(dir.path());
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/nice-work");
    let cfg = GlobalConfig {
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
    let first_session_id = app.sessions[0].claude_session_id.clone();

    // finish it: worktree cleaned up, entry demoted to HISTORY (no live handle).
    app.finish_selected_session();
    assert_eq!(app.sessions.len(), 1);
    assert!(!app.sessions[0].is_live());
    assert_eq!(app.sessions[0].claude_session_id, first_session_id);

    // task goes back to backlog so play_selected accepts it again
    app.store.set_status("TASK-001", Status::Backlog).unwrap();

    // playing the same task again reuses the prior claude session id instead
    // of generating a brand new one, so the conversation resumes.
    app.play_selected();
    assert!(app.status_msg.contains("resumed"), "{}", app.status_msg);
    assert_eq!(
        app.sessions.len(),
        1,
        "the stale history card is replaced in place, not duplicated"
    );
    assert_eq!(app.sessions[0].claude_session_id, first_session_id);
    assert!(app.sessions[0].is_live());

    app.close_selected_session();
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
    let (_tx, job) = fake_job(JobKind::Init);
    app.job = Some(job);
    assert!(app.job_running());

    app.start_init_job("/tmp");
    assert_eq!(app.job.as_ref().unwrap().title, "fake");
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

    let (tx, job) = fake_job(JobKind::Init);
    app.job = Some(job);
    tx.send(JobMsg::Log("working".into())).unwrap();
    app.poll_job();
    assert_eq!(app.job.as_ref().unwrap().logs, vec!["working".to_string()]);
    assert!(!app.job.as_ref().unwrap().finished);

    // the only job kind left (Init) treats `Ok` as the registered project's
    // name, not a bare status message.
    tx.send(JobMsg::Done(Ok("all done".into()))).unwrap();
    app.poll_job();
    assert!(app.job.as_ref().unwrap().finished);
    assert!(
        app.status_msg.contains("project 'all done' registered"),
        "{}",
        app.status_msg
    );
}

#[test]
fn dismiss_job_keeps_running_jobs_in_background() {
    let dir = TmpDir::new("dismiss");
    let mut app = app_with(&dir, &[]);
    let (_tx, job) = fake_job(JobKind::Init);
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

    let (_tx, job) = fake_job(JobKind::Init);
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
    app.apply_intent(Intent::StartInput {
        kind: InputKind::Defer,
        prefill: String::new(),
    });
    assert!(matches!(app.input, Some((InputKind::Defer, ref s)) if s.is_empty()));

    app.apply_intent(Intent::StartInput {
        kind: InputKind::InitPath,
        prefill: "/tmp/somewhere".into(),
    });
    let (kind, buf) = app.input.clone().unwrap();
    assert!(kind == InputKind::InitPath);
    assert_eq!(buf, "/tmp/somewhere");

    app.submit_input(InputKind::Defer, "split the migration".into());
    assert!(app.status_msg.contains("deferred"));
    app.submit_input(InputKind::Convention, "prefer small functions".into());
    assert!(app.status_msg.contains("convention added"));
    app.submit_input(InputKind::NewTask, "quick one".into());
    assert!(app.status_msg.contains("task created"));
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
fn persist_sessions_writes_each_project_to_its_own_file() {
    let dir1 = TmpDir::new("persist-a");
    let dir2 = TmpDir::new("persist-b");
    fs::create_dir_all(dir1.path().join(".backlog")).unwrap();
    fs::create_dir_all(dir2.path().join(".backlog")).unwrap();
    let mut p2 = project(dir2.path(), Vec::new());
    p2.name = "second".into();
    let cfg = GlobalConfig {
        projects: vec![project(dir1.path(), Vec::new()), p2],
    };
    let mut app = App::new(cfg, 0).unwrap();
    open_cat(&mut app, SessionKind::Setup, None, "u-first", dir1.path());
    app.load_project(1);
    open_cat(&mut app, SessionKind::Setup, None, "u-second", dir2.path());
    assert_eq!(app.sessions.len(), 2, "both projects' sessions coexist");

    app.persist_sessions();
    let recs1: Vec<SessionRecord> =
        serde_json::from_str(&fs::read_to_string(dir1.path().join(".jaum/sessions.json")).unwrap())
            .unwrap();
    let recs2: Vec<SessionRecord> =
        serde_json::from_str(&fs::read_to_string(dir2.path().join(".jaum/sessions.json")).unwrap())
            .unwrap();
    // each project's file holds only its OWN session, never the other's.
    assert_eq!(recs1.len(), 1);
    assert_eq!(recs1[0].claude_session_id, "u-first");
    assert_eq!(recs1[0].project, "test");
    assert_eq!(recs2.len(), 1);
    assert_eq!(recs2[0].claude_session_id, "u-second");
    assert_eq!(recs2[0].project, "second");

    // a session tagged with a project no longer in the config (removed since
    // it was persisted) is skipped, not a crash.
    app.sessions.push(SessionEntry::history(&record(
        SessionKind::Setup,
        None,
        "u-ghost",
        dir1.path(),
    )));
    app.sessions.last_mut().unwrap().project = "ghost".into();
    app.persist_sessions();
    let recs1_again: Vec<SessionRecord> =
        serde_json::from_str(&fs::read_to_string(dir1.path().join(".jaum/sessions.json")).unwrap())
            .unwrap();
    assert_eq!(
        recs1_again.len(),
        1,
        "the unknown project's session is skipped, not written anywhere"
    );

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
    app.cleanup_worktrees("test", &None, &[("org/x".into(), dir.path().to_path_buf())]);
    app.cleanup_worktrees(
        "test",
        &Some("TASK-404".into()),
        &[("org/x".into(), dir.path().to_path_buf())],
    );
    // an unknown project resolves to nothing and is also a no-op
    app.cleanup_worktrees(
        "does-not-exist",
        &Some("TASK-001".into()),
        &[("org/x".into(), dir.path().to_path_buf())],
    );
}

// --- sidecar sessions (play over the node stub) -----------------------------

#[test]
fn play_fails_cleanly_when_the_executor_cannot_spawn() {
    let dir = TmpDir::new("spawn-fail");
    let repo = git_repo(dir.path());
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/no-sidecar");
    let cfg = GlobalConfig {
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo,
            }],
        )],
    };
    let mut app = App::new(cfg, 0).unwrap();
    app.executor = ClaudeExecutor::with_bin("/nonexistent-bin-jaum-test");
    app.gh = Gh::with_bin("false");

    app.play_selected();
    assert!(app.status_msg.contains("play failed"), "{}", app.status_msg);
    assert!(app.sessions.is_empty());
    // the spawn failed BEFORE any side effect: no worktree, status untouched
    let wt = app.repos["org/x"]
        .with_file_name("repo.worktrees")
        .join("feat-no-sidecar");
    assert!(
        !wt.exists(),
        "worktree must not be created on spawn failure"
    );
    assert_eq!(app.store.get("TASK-001").unwrap().status, Status::Backlog);
}

#[test]
fn rollback_undoes_worktrees_and_status_of_a_failed_launch() {
    let dir = TmpDir::new("rollback");
    let repo = git_repo(dir.path());
    let backlog = dir.path().join(".backlog");
    write_task(&backlog, "TASK-001", "backlog", "feat/rollback-me");
    let cfg = GlobalConfig {
        projects: vec![project(
            dir.path(),
            vec![RepoMap {
                slug: "org/x".into(),
                path: repo,
            }],
        )],
    };
    let app = App::new(cfg, 0).unwrap();
    // launch creates the worktree and marks wip; the rollback restores both
    let launch = Play::new(
        &app.store,
        &app.git,
        &app.executor,
        &app.work_dir,
        app.repos.clone(),
        app.conventions.clone(),
    )
    .launch("TASK-001")
    .unwrap();
    assert!(launch.worktrees[0].1.exists());
    assert_eq!(app.store.get("TASK-001").unwrap().status, Status::Wip);

    app.rollback_play_launch("TASK-001", &launch.worktrees, Some(Status::Backlog));
    assert!(!launch.worktrees[0].1.exists(), "worktree not removed");
    assert_eq!(app.store.get("TASK-001").unwrap().status, Status::Backlog);
}

#[test]
fn disconnected_sidecar_surfaces_an_error_and_closes_the_turn() {
    let dir = TmpDir::new("disconnect");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let idx = open_sidecar_play(&mut app, "TASK-001", "s-disc", dir.path());
    let (tx, rx) = channel::<SidecarEvent>();
    {
        let chat = app.sessions[idx].chat.as_mut().unwrap();
        chat.rx = Some(rx);
        chat.request_id = Some("s-disc#1".into());
        chat.turn_seq = 1;
    }
    // a routed permission still pending when the sidecar dies belongs to a
    // turn that no longer exists
    app.route_permissions = true;
    app.permissions.track("s-disc", "perm-disc");
    drop(tx);
    app.drain_sidecar();
    assert!(!app.sessions[idx].turn_active(), "turn must close");
    assert_eq!(
        app.permissions.pending_count(),
        0,
        "pending permission survived the disconnect"
    );
    let events = app.sessions[idx].chat.as_ref().unwrap().log.replay();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::Error { category, message }
                if category == "sidecar" && message.contains("disconnected")
        )),
        "disconnect not logged: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::PermissionDecision { permission_id, behavior, .. }
                if permission_id == "perm-disc" && behavior == "deny"
        )),
        "dropped permission not resolved in the log: {events:?}"
    );
    // no deadline left behind: nothing expires and the session stays unblocked
    app.tick_permissions();
    assert!(!app.sessions[idx].blocked);
    let text = app.sessions[idx].parser.screen().contents();
    assert!(text.contains("disconnected"), "not rendered: {text}");
}

#[test]
fn diverging_claude_session_id_migrates_the_log() {
    let dir = TmpDir::new("migrate");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let idx = open_sidecar_play(&mut app, "TASK-001", "old-id", dir.path());
    // history already written under the requested id
    app.sessions[idx]
        .chat
        .as_ref()
        .unwrap()
        .log
        .append(&ChatEvent::TextDelta {
            text: "before the switch".into(),
        })
        .unwrap();
    let (tx, rx) = channel::<SidecarEvent>();
    {
        let chat = app.sessions[idx].chat.as_mut().unwrap();
        chat.rx = Some(rx);
        chat.request_id = Some("old-id#1".into());
        chat.turn_seq = 1;
    }
    tx.send(SidecarEvent::Session {
        request_id: "old-id#1".into(),
        claude_session_id: "new-id".into(),
    })
    .unwrap();
    tx.send(SidecarEvent::Done {
        request_id: "old-id#1".into(),
        usage: None,
        stop_reason: Some("end_turn".into()),
    })
    .unwrap();
    app.drain_sidecar();

    assert_eq!(app.sessions[idx].claude_session_id, "new-id");
    let sessions_dir = app.home().join(".sessions");
    assert!(
        sessions_dir.join("new-id.jsonl").exists(),
        "log not migrated"
    );
    assert!(
        !sessions_dir.join("old-id.jsonl").exists(),
        "old log left behind"
    );
    // the history and the new turn live in the migrated file
    let events = app.sessions[idx].chat.as_ref().unwrap().log.replay();
    assert!(events.contains(&ChatEvent::TextDelta {
        text: "before the switch".into()
    }));
    assert!(events.iter().any(|e| matches!(e, ChatEvent::Done { .. })));
    // the persisted record follows the resumable id
    assert!(
        app.load_session_records()
            .iter()
            .any(|r| r.claude_session_id == "new-id")
    );
}

#[test]
fn render_event_clips_multibyte_text_without_panicking() {
    let dir = TmpDir::new("clip");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let idx = open_sidecar_play(&mut app, "TASK-001", "s-clip", dir.path());
    // 200 two-byte chars: a byte-indexed truncate(120) would split one in half
    let accented = "çãé".repeat(70);
    app.sessions[idx].render_event(&ChatEvent::ToolUse {
        tool_use_id: "tu".into(),
        name: "Bash".into(),
        input: serde_json::json!({ "command": accented.clone() }),
    });
    app.sessions[idx].render_event(&ChatEvent::ToolResult {
        tool_use_id: "tu".into(),
        content: vec![ContentBlock::Text { text: accented }],
        is_error: false,
    });
    assert!(
        app.sessions[idx]
            .parser
            .screen()
            .contents()
            .contains("[tool Bash]")
    );
}

#[test]
fn ensure_sidecar_respawns_a_dead_process() {
    let dir = TmpDir::new("respawn");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);
    app.ensure_sidecar().unwrap();
    app.sidecar.as_mut().unwrap().kill();
    assert!(!app.sidecar.as_mut().unwrap().is_alive());
    app.ensure_sidecar().unwrap();
    assert!(app.sidecar.as_mut().unwrap().is_alive());
}

#[test]
fn sidecar_health_pings_and_restarts_a_hung_process() {
    let dir = TmpDir::new("health");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);

    // no sidecar yet: a tick is a no-op
    app.tick_sidecar_health();
    app.ensure_sidecar().unwrap();

    // within the interval: nothing pinged yet
    app.tick_sidecar_health();
    assert!(app.sidecar_pinged.is_none());

    // force the interval: the tick pings and awaits the pong
    app.last_sidecar_ping = Instant::now() - Duration::from_secs(60);
    app.tick_sidecar_health();
    assert!(app.sidecar_pinged.is_some());

    // the stub answers; the next tick clears the in-flight ping
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.sidecar_pinged.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.tick_sidecar_health();
    }
    assert!(app.sidecar_pinged.is_none(), "pong never cleared the ping");

    // a ping that never comes back kills the process for a lazy respawn
    app.sidecar_pinged = Some(Instant::now());
    app.last_sidecar_ping = Instant::now() - Duration::from_secs(60);
    app.tick_sidecar_health();
    assert!(app.sidecar.is_none());
    assert!(app.status_msg.contains("sidecar unresponsive"));
}

#[test]
fn permission_timeout_without_sidecar_still_blocks_the_session() {
    let dir = TmpDir::new("perm-nosidecar");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    app.permissions = crate::sidecar::PermissionTracker::new(Duration::ZERO);
    open_sidecar_play(&mut app, "TASK-001", "s-noside", dir.path());
    app.permissions.track("s-noside", "perm-1");
    app.tick_permissions();
    assert!(app.sessions[0].blocked);
}

#[test]
fn send_chat_turn_rejects_non_sidecar_sessions() {
    let dir = TmpDir::new("turn-pty");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    open_cat(&mut app, SessionKind::Setup, None, "u-pty", dir.path());
    let err = app.send_chat_turn(0, "hi".into()).unwrap_err();
    assert!(err.to_string().contains("session without a task"));
    app.stop_all_sessions();
}

/// Not used by `play_selected` since the PTY revert (see `open_play_session`'s
/// doc comment), but the sidecar machinery it drives stays live for TASK-011 —
/// these tests exercise it directly instead of through play.
fn play_launch(task: &str, session_id: &str, prompt: &str, cwd: &Path) -> PlayLaunch {
    PlayLaunch {
        id: task.into(),
        session_id: session_id.into(),
        prompt: prompt.into(),
        cwd: cwd.to_path_buf(),
        worktrees: Vec::new(),
        guards: GuardSpec {
            system_prompt_append: String::new(),
            disallowed_tools: Vec::new(),
            guard_patterns: Vec::new(),
            model: "sonnet".into(),
        },
    }
}

/// Drains until the session's turn (and any queued follow-up) settles.
fn drain_until_idle(app: &mut App, idx: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.drain_sidecar();
        let e = &app.sessions[idx];
        let idle = !e.turn_active() && e.chat.as_ref().unwrap().queued.is_empty();
        if idle {
            return;
        }
        assert!(Instant::now() < deadline, "turn(s) never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn open_play_session_starts_a_turn_and_sends_a_queued_message_after_it_finishes() {
    let dir = TmpDir::new("open-play");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);
    let launch = play_launch("TASK-001", "session-a", "hello", dir.path());

    app.open_play_session(&launch, None).unwrap();
    assert_eq!(app.sessions.len(), 1);
    assert!(app.sessions[0].turn_active());
    assert_eq!(app.sessions[0].claude_session_id, "session-a");
    app.sessions[0]
        .chat
        .as_mut()
        .unwrap()
        .queued
        .push_back("second".into());

    drain_until_idle(&mut app, 0);

    let text = app.sessions[0].parser.screen().contents();
    assert!(
        text.contains("echo:hello") && text.contains("echo:second"),
        "queued message not sent: {text}"
    );
}

#[test]
fn open_play_session_with_resume_replays_the_log_and_replaces_the_existing_entry() {
    let dir = TmpDir::new("open-play-resume");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);
    let first = play_launch("TASK-001", "session-r", "hello", dir.path());
    app.open_play_session(&first, None).unwrap();
    drain_until_idle(&mut app, 0);

    let again = play_launch("TASK-001", "session-r", "again", dir.path());
    app.open_play_session(&again, Some("session-r".into()))
        .unwrap();
    assert_eq!(app.sessions.len(), 1, "resume must replace, not duplicate");
    drain_until_idle(&mut app, 0);

    let text = app.sessions[0].parser.screen().contents();
    assert!(
        text.contains("echo:hello") && text.contains("echo:again"),
        "replayed history missing prior turn: {text}"
    );
}

#[test]
fn permission_request_is_auto_allowed_and_logged_when_routing_is_off() {
    let dir = TmpDir::new("auto-allow");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);
    assert!(!app.route_permissions);
    let launch = play_launch("TASK-001", "session-c", "ask-permission", dir.path());
    app.open_play_session(&launch, None).unwrap();

    drain_until_idle(&mut app, 0);

    let events = app.sessions[0].chat.as_ref().unwrap().log.replay();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::PermissionDecision { behavior, .. } if behavior == "allow"
        )),
        "auto-allow not logged: {events:?}"
    );
}

#[test]
fn permission_request_is_tracked_for_routing_when_enabled() {
    let dir = TmpDir::new("route-perm");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    sidecar_stub(&mut app);
    app.route_permissions = true;
    let launch = play_launch("TASK-001", "session-d", "ask-permission", dir.path());
    app.open_play_session(&launch, None).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while app.permissions.pending_count() == 0 {
        app.drain_sidecar();
        assert!(
            Instant::now() < deadline,
            "permission request never arrived"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.permissions.pending_count(), 1);
}
