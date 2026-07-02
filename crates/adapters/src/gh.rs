use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use jaum_core::MergeState;

/// `gh` adapter via shell-out. GitHub is downstream: the tool creates PRs and
/// **reads** the number and merge state, never merges.
///
/// `gh` runs INSIDE the repo directory (`dir`) and auto-detects the GitHub
/// repository from the remote — so we don't depend on the internal slug being
/// in `owner/name` form (projects whose slug is just the folder name also work).
pub struct Gh {
    bin: String,
}

impl Default for Gh {
    fn default() -> Self {
        Self::new()
    }
}

impl Gh {
    pub fn new() -> Self {
        Self {
            bin: "gh".to_string(),
        }
    }

    /// Points to an alternative binary (used in tests).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Opens a PR for `branch`, running `gh` in the repo directory. Returns the
    /// number. NEVER merges — only creates.
    pub fn pr_create(&self, dir: &Path, branch: &str) -> Result<u64> {
        let out = self.run(dir, &["pr", "create", "--head", branch, "--fill"])?;
        parse_pr_number_from_url(&out)
            .with_context(|| format!("extracting PR number from gh output: {out:?}"))
    }

    /// Number of the open/closed PR for `branch`, or `0` if none exists.
    pub fn pr_number(&self, dir: &Path, branch: &str) -> Result<u64> {
        let out = self.run(
            dir,
            &[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "all",
                "--json",
                "number",
                "--jq",
                ".[0].number // 0",
            ],
        )?;
        let s = out.trim();
        if s.is_empty() {
            return Ok(0);
        }
        s.parse()
            .with_context(|| format!("parsing PR number: {s:?}"))
    }

    /// PR merge state. `pr == 0` is treated as `NotCreated` without calling gh.
    pub fn pr_merge_state(&self, dir: &Path, pr: u64) -> Result<MergeState> {
        if pr == 0 {
            return Ok(MergeState::NotCreated);
        }
        let out = self.run(
            dir,
            &[
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
            ],
        )?;
        Ok(match out.trim() {
            "MERGED" => MergeState::Merged,
            "OPEN" => MergeState::Open,
            "CLOSED" => MergeState::Closed,
            _ => MergeState::Unknown,
        })
    }

    /// PR diff (by number), pulled from GitHub. Empty if `pr == 0`.
    pub fn pr_diff(&self, dir: &Path, pr: u64) -> Result<String> {
        if pr == 0 {
            return Ok(String::new());
        }
        self.run(dir, &["pr", "diff", &pr.to_string()])
    }

    /// PR title + body (by number). `("", "")` if `pr == 0`.
    pub fn pr_view(&self, dir: &Path, pr: u64) -> Result<(String, String)> {
        if pr == 0 {
            return Ok((String::new(), String::new()));
        }
        let out = self.run(
            dir,
            &["pr", "view", &pr.to_string(), "--json", "title,body"],
        )?;
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        Ok((get("title"), get("body")))
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .current_dir(dir)
            .args(args)
            .output()
            .with_context(|| format!("running {} {args:?} in {}", self.bin, dir.display()))?;
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

/// Extracts the PR number from the URL emitted by `gh pr create`. gh may print
/// warnings before the URL, so we use the last non-empty line.
fn parse_pr_number_from_url(s: &str) -> Option<u64> {
    let line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    line.trim().rsplit('/').next()?.parse().ok()
}
