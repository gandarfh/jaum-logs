use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Git;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-git-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(repo: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

/// Repo fixture with default branch `main` and an initial commit.
fn init_repo(dir: &TmpDir) -> PathBuf {
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main", "-q"]);
    git(&repo, &["config", "user.email", "t@test.dev"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "line one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

#[test]
fn branch_create_is_idempotent() {
    let dir = TmpDir::new("branch");
    let repo = init_repo(&dir);
    let g = Git::new();

    g.branch_create(&repo, "feat/x").unwrap();
    // second time must not fail
    g.branch_create(&repo, "feat/x").unwrap();

    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", "feat/x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("feat/x"));
}

#[test]
fn worktree_add_creates_and_remove_deletes() {
    let dir = TmpDir::new("worktree");
    let repo = init_repo(&dir);
    let g = Git::new();

    let wt = g.worktree_add(&repo, "feat/task-012").unwrap();
    assert!(wt.exists(), "worktree was not created");
    assert!(
        wt.join("README.md").exists(),
        "checkout did not bring files"
    );
    // the directory name sanitizes the `/`
    assert_eq!(wt.file_name().unwrap().to_str().unwrap(), "feat-task-012");

    g.worktree_remove(&repo, "feat/task-012").unwrap();
    assert!(!wt.exists(), "worktree was not removed");
}

#[test]
fn worktree_add_on_existing_branch_works() {
    let dir = TmpDir::new("worktree-existing");
    let repo = init_repo(&dir);
    let g = Git::new();

    g.branch_create(&repo, "feat/y").unwrap();
    let wt = g.worktree_add(&repo, "feat/y").unwrap();
    assert!(wt.exists());
}

#[test]
fn worktree_add_is_idempotent_when_already_exists() {
    // re-playing a wip task whose worktree was preserved by a shutdown: the
    // second add must not fail, just return the same path.
    let dir = TmpDir::new("worktree-idem");
    let repo = init_repo(&dir);
    let g = Git::new();

    let wt1 = g.worktree_add(&repo, "feat/z").unwrap();
    assert!(wt1.exists());
    let wt2 = g.worktree_add(&repo, "feat/z").unwrap();
    assert_eq!(wt1, wt2, "must reuse the existing worktree");
    assert!(wt2.exists());
}

#[test]
fn diff_shows_branch_changes() {
    let dir = TmpDir::new("diff");
    let repo = init_repo(&dir);
    let g = Git::new();

    // create branch + worktree, commit a change there; objects are shared
    let wt = g.worktree_add(&repo, "feat/change").unwrap();
    fs::write(wt.join("README.md"), "line one\nline two\n").unwrap();
    git(&wt, &["config", "user.email", "t@test.dev"]);
    git(&wt, &["config", "user.name", "Test"]);
    git(&wt, &["commit", "-aqm", "change readme"]);

    let diff = g.diff(&repo, "feat/change").unwrap();
    assert!(
        diff.contains("line two"),
        "diff did not reflect the change:\n{diff}"
    );
    assert!(diff.contains("README.md"));
}

#[test]
fn diff_on_branch_without_changes_is_empty() {
    let dir = TmpDir::new("diff-empty");
    let repo = init_repo(&dir);
    let g = Git::new();
    g.branch_create(&repo, "feat/nothing").unwrap();
    let diff = g.diff(&repo, "feat/nothing").unwrap();
    assert!(diff.trim().is_empty());
}

#[test]
fn default_uses_the_real_git_binary_name() {
    let dir = TmpDir::new("default");
    let repo = init_repo(&dir);
    let g = Git::default();
    g.branch_create(&repo, "feat/via-default").unwrap();
}

#[test]
fn missing_binary_errors_with_context() {
    let dir = TmpDir::new("nobin");
    let g = Git::with_bin("/does/not/exist/git");
    let err = g.branch_create(&dir.0, "feat/x").unwrap_err();
    assert!(err.to_string().contains("rev-parse"));
}

#[test]
fn diff_uses_origin_head_as_base_when_present() {
    let dir = TmpDir::new("origin-head");
    let repo = init_repo(&dir);
    // simulate a clone: origin/main exists and origin/HEAD points at it
    git(&repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    let g = Git::new();
    let wt = g.worktree_add(&repo, "feat/from-origin").unwrap();
    fs::write(wt.join("new.txt"), "hello\n").unwrap();
    git(&wt, &["config", "user.email", "t@test.dev"]);
    git(&wt, &["config", "user.name", "Test"]);
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "add file"]);

    let diff = g.diff(&repo, "feat/from-origin").unwrap();
    assert!(diff.contains("new.txt"));
}

#[test]
fn diff_accepts_origin_head_outside_origin_namespace() {
    let dir = TmpDir::new("odd-head");
    let repo = init_repo(&dir);
    // origin/HEAD pointing at a non-origin remote: used verbatim as the base
    git(&repo, &["update-ref", "refs/remotes/upstream/dev", "main"]);
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/upstream/dev",
        ],
    );
    let g = Git::new();
    g.branch_create(&repo, "feat/odd").unwrap();
    let diff = g.diff(&repo, "feat/odd").unwrap();
    assert!(diff.trim().is_empty());
}

#[test]
fn default_branch_falls_back_to_master() {
    let dir = TmpDir::new("master");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "master", "-q"]);
    git(&repo, &["config", "user.email", "t@test.dev"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let g = Git::new();
    g.branch_create(&repo, "feat/m").unwrap();
    let diff = g.diff(&repo, "feat/m").unwrap();
    assert!(diff.trim().is_empty());
}

#[test]
fn diff_fails_when_no_default_branch_is_detectable() {
    let dir = TmpDir::new("nodefault");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "trunk", "-q"]);
    git(&repo, &["config", "user.email", "t@test.dev"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let g = Git::new();
    let err = g.diff(&repo, "trunk").unwrap_err();
    assert!(
        err.to_string()
            .contains("could not detect the default branch"),
        "unexpected error: {err}"
    );
}
