use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Session};
use jaum_core::Store;
use jaum_flows::setup::{Setup, branch_leaks_id, is_template};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-setup-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FakeExec;
impl Executor for FakeExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

#[test]
fn branch_leaks_id_catches_task_pattern() {
    assert!(branch_leaks_id("feat/task-001"));
    assert!(branch_leaks_id("TASK-12-fix"));
    assert!(branch_leaks_id("chore/refactor-task-3"));
    // branches that describe the work don't trigger
    assert!(!branch_leaks_id("feat/markdown-deck-parser"));
    assert!(!branch_leaks_id("fix/task-runner-timeout")); // "task-runner", no digit
}

#[test]
fn is_template_detects_empty_and_scaffold() {
    assert!(is_template(""));
    assert!(is_template(
        "# Conventions\n\nOne per line (use `-`).\n\n- \n"
    ));
    assert!(!is_template("# Conventions\n\n- do not reference RFC numbers in comments\n"));
}

#[test]
fn build_prompt_brings_tasks_repos_and_required_setup() {
    let dir = TmpDir::new("prompt");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    // task with no prs (as it's born from ingest)
    fs::write(
        backlog.join("TASK-001.md"),
        "---\nid: TASK-001\ntype: impl\nstatus: backlog\nrfcs: [RFC-0001]\n---\n\n## Objective\nimplement the parser\n",
    )
    .unwrap();
    let store = Store::new(&backlog);
    let repos = HashMap::from([("org/app".to_string(), dir.0.join("repo"))]);

    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), repos, "");
    let p = setup.build_prompt().unwrap();

    assert!(p.contains("Contract")); // states the contract positively
    assert!(p.contains("Your work"));
    assert!(p.contains("TASK-001"));
    assert!(p.contains("NO repo")); // flags the missing link
    assert!(p.contains("org/app")); // available slug
    assert!(p.contains("template")); // conventions empty
    assert!(p.contains("prs")); // link schema
}

#[test]
fn build_prompt_flags_branch_with_leaked_id() {
    let dir = TmpDir::new("leak");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(
        backlog.join("TASK-001.md"),
        "---\nid: TASK-001\ntype: impl\nstatus: backlog\nprs:\n  - repo: org/app\n    pr: 0\n    branch: feat/task-001\n---\n\n## Objective\nx\n",
    )
    .unwrap();
    let store = Store::new(&backlog);
    let repos = HashMap::from([("org/app".to_string(), dir.0.join("repo"))]);
    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), repos, "");
    let p = setup.build_prompt().unwrap();
    assert!(p.contains("leaks the id"));
}

#[test]
fn start_opens_interactive_session() {
    let dir = TmpDir::new("start");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    let store = Store::new(&backlog);
    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), HashMap::new(), "");
    // must not panic building the session (cat as a fake executor)
    let (mut s, uuid) = setup.start().unwrap();
    assert!(!uuid.is_empty(), "start must return a session-id");
    s.write_input(&[0x04]).unwrap();
    assert!(s.wait().unwrap());
}
