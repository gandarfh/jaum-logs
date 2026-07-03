use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Git;
use jaum_core::{Status, Store, Task};
use jaum_flows::play::{
    GuardSpec, HookGuard, Play, build_prompt, guard_spec, merge_disallowed, reinjection_text,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-play-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const FIXTURE: &str = r#"---
id: TASK-001
type: impl
status: ready
rfcs: [RFC-003]
adrs: [ADR-011]
prs:
  - repo: myorg/repo
    pr: 0
    branch: feat/task-001
constraints:
  - text: "do not touch src/legacy/"
    enforce: hook
  - text: "do not run migration"
    enforce: hook
  - text: "keep API stable"
    enforce: review
---

## Objective
Implement an open enum.

## Acceptance criteria
- [ ] done
"#;

fn task() -> Task {
    let dir = TmpDir::new("task");
    let store = Store::new(&dir.0);
    fs::write(dir.0.join("TASK-001.md"), FIXTURE).unwrap();
    store.get("TASK-001").unwrap()
}

// --- pure parts -----------------------------------------------------------

#[test]
fn build_prompt_includes_objective_context_and_constraints() {
    let p = build_prompt(&task(), "");
    assert!(p.contains("Implement an open enum"));
    assert!(p.contains("RFC-003"));
    assert!(p.contains("ADR-011"));
    assert!(p.contains("keep API stable")); // enforce: review shows up in the body
    assert!(p.contains("NEVER merge"));
}

#[test]
fn guard_spec_blocks_merge_and_injects_constraints() {
    let g = guard_spec(&task(), "");
    assert!(g.disallowed_tools.iter().any(|t| t.contains("git merge")));
    assert!(g.disallowed_tools.iter().any(|t| t.contains("gh pr merge")));
    assert_eq!(g.disallowed_tools, merge_disallowed());
    assert!(g.system_prompt_append.contains("do not touch src/legacy/"));
    assert!(g.system_prompt_append.contains("keep API stable"));
    assert_eq!(g.model, jaum_flows::AGENT_MODEL);
}

#[test]
fn guard_spec_derives_one_pattern_per_hook_constraint() {
    let g = guard_spec(&task(), "");
    assert_eq!(
        g.guard_patterns,
        vec![
            HookGuard {
                pattern: "src/legacy/".into(),
                reason: "do not touch src/legacy/".into(),
            },
            HookGuard {
                pattern: "migration".into(),
                reason: "do not run migration".into(),
            },
        ],
        "enforce: review constraints never become guard patterns"
    );
}

#[test]
fn reinjection_includes_project_conventions() {
    let t = reinjection_text(&task(), "- do not reference RFC numbers in comments");
    assert!(t.contains("Project conventions"));
    assert!(t.contains("do not reference RFC numbers"));
    // and still carries the task constraints
    assert!(t.contains("do not run migration"));
}

#[test]
fn reinjection_separates_mechanical_from_semantic() {
    let t = reinjection_text(&task(), "");
    assert!(t.contains("Mechanically blocked"));
    assert!(t.contains("do not run migration"));
    assert!(t.contains("Your responsibility"));
    assert!(t.contains("keep API stable"));
}

#[test]
fn reinjection_defines_repo_output_conventions() {
    let t = reinjection_text(&task(), "");
    assert!(t.contains("ENGLISH")); // PR/commits in English
    assert!(t.contains("em dashes")); // no em dashes, pragmatic style
    assert!(t.contains("Generated with Claude Code")); // forbids AI attribution
}

// --- launch (git fixture, no executor) -------------------------------------

fn git_init(repo: &Path) {
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    fs::create_dir_all(repo).unwrap();
    run(&["init", "-b", "main", "-q"]);
    run(&["config", "user.email", "t@test.dev"]);
    run(&["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "x\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "init"]);
}

struct LaunchFixture {
    _root: TmpDir,
    store: Store,
    repos: std::collections::HashMap<String, PathBuf>,
}

fn launch_fixture(tag: &str, task_md: &str) -> LaunchFixture {
    let root = TmpDir::new(tag);
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), task_md).unwrap();
    let repos_root = root.0.join("repos");
    git_init(&repos_root.join("repo"));
    LaunchFixture {
        store: Store::new(&backlog),
        repos: std::collections::HashMap::from([(
            "myorg/repo".to_string(),
            repos_root.join("repo"),
        )]),
        _root: root,
    }
}

#[test]
fn launch_creates_worktree_marks_wip_and_returns_the_guarded_turn() {
    let fx = launch_fixture("launch", FIXTURE);
    let git = Git::new();
    let play = Play::new(&fx.store, &git, fx.repos.clone(), String::new());

    let launch = play.launch("TASK-001").unwrap();

    assert!(launch.worktrees[0].1.exists(), "worktree not created");
    assert_eq!(launch.cwd, launch.worktrees[0].1);
    assert_eq!(fx.store.get("TASK-001").unwrap().status, Status::Wip);
    assert!(launch.prompt.contains("Implement an open enum"));
    assert_eq!(launch.id, "TASK-001");
    // the session uuid keys the log and the claude session id
    assert_eq!(
        uuid::Uuid::parse_str(&launch.session_id)
            .unwrap()
            .get_version_num(),
        4
    );
    assert_eq!(
        launch.guards,
        guard_spec(&fx.store.get("TASK-001").unwrap(), "")
    );

    play.cleanup("TASK-001", &launch.worktrees).unwrap();
    assert!(
        !launch.worktrees[0].1.exists(),
        "worktree not removed on cleanup"
    );
}

#[test]
fn resume_spec_recomputes_the_guards_from_disk() {
    let fx = launch_fixture("respec", FIXTURE);
    let git = Git::new();
    let play = Play::new(&fx.store, &git, fx.repos.clone(), "- new convention");
    let spec: GuardSpec = play.resume_spec("TASK-001").unwrap();
    assert!(spec.system_prompt_append.contains("new convention"));
    assert!(spec.system_prompt_append.contains("do not run migration"));
    assert!(!spec.guard_patterns.is_empty());
}

const FIXTURE_BARE: &str = r#"---
id: TASK-002
type: impl
status: ready
---

## Objective
Just do it.
"#;

fn bare_task() -> Task {
    let dir = TmpDir::new("bare");
    let store = Store::new(&dir.0);
    fs::write(dir.0.join("TASK-002.md"), FIXTURE_BARE).unwrap();
    store.get("TASK-002").unwrap()
}

#[test]
fn build_prompt_without_refs_or_constraints_skips_sections() {
    let p = build_prompt(&bare_task(), "");
    assert!(p.contains("Just do it."));
    assert!(!p.contains("## Context"), "no refs -> no context section");
    let t = reinjection_text(&bare_task(), "");
    assert!(!t.contains("Mechanically blocked"));
    assert!(!t.contains("Your responsibility"));
    assert!(t.contains("do NOT merge")); // fixed reminder always present
}

#[test]
fn launch_rejects_task_without_linked_prs() {
    let root = TmpDir::new("noprs");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-002.md"), FIXTURE_BARE).unwrap();

    let store = Store::new(&backlog);
    let git = Git::new();
    let play = Play::new(
        &store,
        &git,
        std::collections::HashMap::new(),
        String::new(),
    );

    let err = match play.launch("TASK-002") {
        Ok(_) => panic!("should reject a task without prs"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("no repo/branch linked"));
}

#[test]
fn launch_rejects_spike() {
    let root = TmpDir::new("spike");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    let spike = FIXTURE.replace("type: impl", "type: spike");
    fs::write(backlog.join("TASK-001.md"), spike).unwrap();

    let store = Store::new(&backlog);
    let git = Git::new();
    let play = Play::new(
        &store,
        &git,
        std::collections::HashMap::new(),
        String::new(),
    );

    let err = match play.launch("TASK-001") {
        Ok(_) => panic!("should reject spike"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("spike"));
}

#[test]
fn launch_rejects_unmapped_repo() {
    let root = TmpDir::new("unmapped");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();

    let store = Store::new(&backlog);
    let git = Git::new();
    let play = Play::new(
        &store,
        &git,
        std::collections::HashMap::new(),
        String::new(),
    );

    let err = match play.launch("TASK-001") {
        Ok(_) => panic!("should reject an unmapped repo"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("not mapped"));
}

#[test]
fn cleanup_skips_worktree_whose_link_was_removed() {
    let fx = launch_fixture("cleanupgone", FIXTURE);
    let git = Git::new();
    let play = Play::new(&fx.store, &git, fx.repos.clone(), String::new());
    let launch = play.launch("TASK-001").unwrap();

    // the task loses its prs link before cleanup: nothing to remove for that repo
    let unlinked = FIXTURE.replace("status: ready", "status: wip");
    let unlinked = unlinked
        .lines()
        .filter(|l| {
            !(l.starts_with("prs:")
                || l.contains("repo: myorg/repo")
                || l.contains("pr: 0")
                || l.contains("branch: feat/task-001"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(fx.store.get("TASK-001").unwrap().path.unwrap(), unlinked).unwrap();

    play.cleanup("TASK-001", &launch.worktrees).unwrap();
    assert!(
        launch.worktrees[0].1.exists(),
        "worktree stays when the link is gone"
    );
}
