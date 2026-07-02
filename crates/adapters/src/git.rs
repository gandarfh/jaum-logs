use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// `git` adapter via shell-out. Operates on a repository's local directory
/// (`repo`). Mapping the "owner/name" slug -> local path is the orchestrator's
/// (play's) responsibility, not this adapter's.
pub struct Git {
    bin: String,
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    pub fn new() -> Self {
        Self {
            bin: "git".to_string(),
        }
    }

    /// Points to an alternative binary (used in tests).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Creates the branch from the current HEAD. Idempotent: no-op if it exists.
    pub fn branch_create(&self, repo: &Path, branch: &str) -> Result<()> {
        if self.branch_exists(repo, branch)? {
            return Ok(());
        }
        self.run(repo, &["branch", branch])?;
        Ok(())
    }

    /// Creates a worktree for `branch` (creating the branch if it doesn't exist
    /// yet). Returns the worktree path. Idempotent: if the worktree already
    /// exists at the deterministic path, reuse it (e.g. re-playing a wip task
    /// whose worktree was preserved by an earlier shutdown).
    pub fn worktree_add(&self, repo: &Path, branch: &str) -> Result<PathBuf> {
        let path = self.worktree_path(repo, branch);
        if path.exists() {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating worktrees directory {}", parent.display()))?;
        }
        let path_str = path.to_string_lossy().into_owned();
        if self.branch_exists(repo, branch)? {
            self.run(repo, &["worktree", "add", &path_str, branch])?;
        } else {
            self.run(repo, &["worktree", "add", "-b", branch, &path_str])?;
        }
        Ok(path)
    }

    /// Removes `branch`'s worktree. Doesn't use `--force`: a dirty worktree
    /// fails, so we don't discard work silently.
    pub fn worktree_remove(&self, repo: &Path, branch: &str) -> Result<()> {
        let path = self.worktree_path(repo, branch);
        let path_str = path.to_string_lossy().into_owned();
        self.run(repo, &["worktree", "remove", &path_str])?;
        Ok(())
    }

    /// Diff of `branch` against the default branch (three-dot: changes since the
    /// divergence). This is the PR diff, used by review.
    pub fn diff(&self, repo: &Path, branch: &str) -> Result<String> {
        let base = self.default_branch(repo)?;
        let spec = format!("{base}...{branch}");
        self.run(repo, &["diff", &spec])
    }

    // --- internals ---------------------------------------------------------

    fn branch_exists(&self, repo: &Path, branch: &str) -> Result<bool> {
        let out = Command::new(&self.bin)
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .with_context(|| format!("running {} rev-parse", self.bin))?;
        Ok(out.status.success())
    }

    /// Detects the default branch: `origin/HEAD`, else `main`, else `master`.
    fn default_branch(&self, repo: &Path) -> Result<String> {
        if let Ok(out) = self.run(
            repo,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        ) {
            let s = out.trim();
            if let Some(b) = s.strip_prefix("origin/") {
                return Ok(b.to_string());
            }
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
        for cand in ["main", "master"] {
            if self.branch_exists(repo, cand)? {
                return Ok(cand.to_string());
            }
        }
        bail!(
            "could not detect the default branch of {}",
            repo.display()
        )
    }

    /// Worktree path: sibling of the repo, outside the main working tree.
    /// `feat/task-012` becomes `feat-task-012` to be a valid directory name.
    fn worktree_path(&self, repo: &Path, branch: &str) -> PathBuf {
        let safe = branch.replace('/', "-");
        let name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
        repo.with_file_name(format!("{name}.worktrees")).join(safe)
    }

    fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .with_context(|| format!("running {} {args:?}", self.bin))?;
        if !out.status.success() {
            bail!(
                "{} {args:?} failed: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
