use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Git, Session};
use jaum_core::{Status, Store, Task};
use jaum_flows::play::{Play, build_prompt, guard_flags, pretool_hook_script, reinjection_text};

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
fn guard_flags_blocks_merge_and_injects_constraints() {
    let f = guard_flags(&task(), "");
    assert!(f.disallowed_tools.iter().any(|t| t.contains("git merge")));
    assert!(f.disallowed_tools.iter().any(|t| t.contains("gh pr merge")));
    let sys = f.append_system_prompt.unwrap();
    assert!(sys.contains("do not touch src/legacy/"));
    assert!(sys.contains("keep API stable"));
    assert_eq!(f.model.as_deref(), Some(jaum_flows::AGENT_MODEL));
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

#[test]
fn settings_disables_ai_co_author() {
    use std::path::Path;
    let s = jaum_flows::play::settings_json(Path::new("/p/pre.sh"), Path::new("/p/re.txt"));
    assert_eq!(s["includeCoAuthoredBy"], serde_json::json!(false));
}

// --- real PreToolUse hook execution ---------------------------------------

fn run_hook(script: &str, stdin_json: &str) -> String {
    let dir = TmpDir::new("hook");
    let path = dir.0.join("pretool.sh");
    fs::write(&path, script).unwrap();
    let mut child = Command::new("bash")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn hook_blocks_merge_always() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Bash","tool_input":{"command":"git merge main"}}"#,
    );
    assert!(
        out.contains("\"permissionDecision\":\"deny\""),
        "out: {out}"
    );
    assert!(out.contains("merge"));
}

#[test]
fn hook_blocks_path_constraint() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/legacy/foo.rs"}}"#,
    );
    assert!(out.contains("deny"), "should block src/legacy/; out: {out}");
    assert!(out.contains("src/legacy/"));
}

#[test]
fn hook_blocks_keyword_constraint() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Bash","tool_input":{"command":"npm run migration"}}"#,
    );
    assert!(out.contains("deny"), "should block migration; out: {out}");
}

#[test]
fn hook_allows_action_that_matches_no_constraint() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/main.rs"}}"#,
    );
    assert!(
        out.trim().is_empty(),
        "should not block src/main.rs; out: {out}"
    );
}

#[test]
fn hook_does_not_block_enforce_review_constraint() {
    // "keep API stable" is enforce: review -> hook does NOT catch it (detected in review)
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/api.rs"}}"#,
    );
    assert!(
        out.trim().is_empty(),
        "review constraints don't go through the hook; out: {out}"
    );
}

// --- start (fake executor over `cat` + git fixture) -----------------------

struct Rec {
    calls: RefCell<Vec<(String, ExecFlags)>>,
}
impl Executor for Rec {
    fn spawn_oneshot(&self, prompt: &str, flags: &ExecFlags) -> anyhow::Result<String> {
        self.calls
            .borrow_mut()
            .push((prompt.to_string(), flags.clone()));
        Ok(String::new())
    }
    fn spawn_interactive(&self, prompt: &str, flags: &ExecFlags) -> anyhow::Result<Session> {
        self.calls
            .borrow_mut()
            .push((prompt.to_string(), flags.clone()));
        // real session over `cat`, just to return a valid Session
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

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

#[test]
fn start_creates_worktree_installs_hooks_and_marks_wip() {
    let root = TmpDir::new("start");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();
    let repos_root = root.0.join("repos");
    git_init(&repos_root.join("repo"));

    let store = Store::new(&backlog);
    let git = Git::new();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let repos =
        std::collections::HashMap::from([("myorg/repo".to_string(), repos_root.join("repo"))]);
    let play = Play::new(
        &store,
        &git,
        &rec,
        root.0.join(".jaum"),
        repos,
        String::new(),
    );

    let mut ps = play.start("TASK-001").unwrap();

    // worktree and artifacts on disk
    assert!(ps.worktrees[0].1.exists(), "worktree not created");
    assert!(ps.artifacts.settings_path.exists());
    assert!(ps.artifacts.pretool_path.exists());

    // status became wip
    assert_eq!(store.get("TASK-001").unwrap().status, Status::Wip);

    // correct flags and prompt reached the executor
    let calls = rec.calls.borrow();
    let (prompt, flags) = &calls[0];
    assert!(prompt.contains("Implement an open enum"));
    assert!(
        flags
            .disallowed_tools
            .iter()
            .any(|t| t.contains("git merge"))
    );
    assert!(flags.settings.is_some(), "hook (--settings) not applied");
    assert!(flags.cwd.is_some(), "cwd (worktree) not applied");
    drop(calls);

    play.stop(&mut ps).unwrap();
    assert!(!ps.worktrees[0].1.exists(), "worktree not removed on stop");
}

#[test]
fn start_injects_session_id_and_resume_resumes_without_prompt() {
    let root = TmpDir::new("resume");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();
    let repos_root = root.0.join("repos");
    git_init(&repos_root.join("repo"));

    let store = Store::new(&backlog);
    let git = Git::new();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let repos =
        std::collections::HashMap::from([("myorg/repo".to_string(), repos_root.join("repo"))]);
    let play = Play::new(
        &store,
        &git,
        &rec,
        root.0.join(".jaum"),
        repos,
        String::new(),
    );

    let ps = play.start("TASK-001").unwrap();
    // start injects --session-id with the returned uuid (not --resume)
    {
        let calls = rec.calls.borrow();
        let (_prompt, flags) = &calls[0];
        assert_eq!(
            flags.session_id.as_deref(),
            Some(ps.claude_session_id.as_str())
        );
        assert!(flags.resume.is_none(), "start must not use --resume");
    }

    // resume injects --resume <uuid>, no positional prompt, no session_id
    let cwd = ps.worktrees[0].1.clone();
    let _ = play
        .resume("TASK-001", &ps.claude_session_id, &cwd)
        .unwrap();
    let calls = rec.calls.borrow();
    let (prompt, flags) = calls.last().unwrap();
    assert!(
        prompt.is_empty(),
        "resume does not resend the initial prompt"
    );
    assert_eq!(flags.resume.as_deref(), Some(ps.claude_session_id.as_str()));
    assert!(
        flags.session_id.is_none(),
        "resume does not use --session-id"
    );
    assert!(flags.cwd.is_some(), "resume keeps the worktree cwd");
}

#[test]
fn start_rejects_spike() {
    let root = TmpDir::new("spike");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    let spike = FIXTURE.replace("type: impl", "type: spike");
    fs::write(backlog.join("TASK-001.md"), spike).unwrap();

    let store = Store::new(&backlog);
    let git = Git::new();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let play = Play::new(
        &store,
        &git,
        &rec,
        root.0.join(".jaum"),
        std::collections::HashMap::new(),
        String::new(),
    );

    let err = match play.start("TASK-001") {
        Ok(_) => panic!("should reject spike"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("spike"));
}
