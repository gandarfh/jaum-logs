use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_core::Store;
use jaum_flows::conflict::Conflict;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-conflict-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn task_md(id: &str, status: &str, repo: &str, branch: &str) -> String {
    format!(
        "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: {repo}\n    pr: 0\n    branch: {branch}\n---\n\n## Objective\nx\n"
    )
}

fn store_with(dir: &TmpDir, tasks: &[(&str, &str, &str, &str)]) -> Store {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for (id, status, repo, branch) in tasks {
        fs::write(
            backlog.join(format!("{id}.md")),
            task_md(id, status, repo, branch),
        )
        .unwrap();
    }
    Store::new(&backlog)
}

#[test]
fn detect_overlap_flags_wip_in_same_repo() {
    let dir = TmpDir::new("overlap");
    let store = store_with(
        &dir,
        &[
            ("TASK-001", "wip", "org/parser", "feat/a"),
            ("TASK-002", "wip", "org/parser", "feat/b"),
            ("TASK-003", "wip", "org/runtime", "feat/c"),
        ],
    );
    let conflict = Conflict::new(&store);

    let overlaps = conflict.detect_overlap().unwrap();
    assert_eq!(overlaps.len(), 1);
    assert_eq!(
        overlaps[0],
        ("TASK-001".into(), "TASK-002".into(), "org/parser".into())
    );
}

#[test]
fn detect_overlap_ignores_non_wip() {
    let dir = TmpDir::new("overlap-status");
    let store = store_with(
        &dir,
        &[
            ("TASK-001", "wip", "org/parser", "feat/a"),
            ("TASK-002", "review", "org/parser", "feat/b"), // not wip
        ],
    );
    let conflict = Conflict::new(&store);
    assert!(conflict.detect_overlap().unwrap().is_empty());
}

#[test]
fn active_sessions_lists_wip() {
    let dir = TmpDir::new("active");
    let store = store_with(
        &dir,
        &[
            ("TASK-001", "wip", "org/parser", "feat/a"),
            ("TASK-002", "backlog", "org/x", "feat/b"),
        ],
    );
    let conflict = Conflict::new(&store);
    let sessions = conflict.active_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "TASK-001");
    assert_eq!(sessions[0].repos, vec!["org/parser"]);
    assert_eq!(sessions[0].branches, vec!["feat/a"]);
}

#[test]
fn lock_acquire_excludes_other_task_and_release_frees() {
    let dir = TmpDir::new("lock");
    let store = store_with(
        &dir,
        &[
            ("TASK-001", "wip", "org/x", "feat/a"),
            ("TASK-002", "wip", "org/y", "feat/b"),
        ],
    );
    let conflict = Conflict::new(&store);

    conflict.lock_acquire("TASK-001", "port:8080").unwrap();
    assert_eq!(
        conflict.held_locks().unwrap(),
        vec![("port:8080".to_string(), "TASK-001".to_string())]
    );

    // another task can't grab the same resource
    let err = conflict.lock_acquire("TASK-002", "port:8080").unwrap_err();
    assert!(err.to_string().contains("is already locked by TASK-001"));

    // re-acquire by the same task is idempotent
    conflict.lock_acquire("TASK-001", "port:8080").unwrap();
    assert_eq!(conflict.held_locks().unwrap().len(), 1);

    // release frees it for the other task
    conflict.lock_release("TASK-001", "port:8080").unwrap();
    assert!(conflict.held_locks().unwrap().is_empty());
    conflict.lock_acquire("TASK-002", "port:8080").unwrap();
    assert_eq!(conflict.held_locks().unwrap()[0].1, "TASK-002");
}
