use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use jaum_core::{CiStatus, MergeState, PrCi};

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

    /// PR state + aggregated CI checks + head commit, in one gh call.
    /// `pr == 0` short-circuits to `NotCreated`/`Unknown` without calling gh.
    pub fn pr_ci(&self, dir: &Path, pr: u64) -> Result<PrCi> {
        if pr == 0 {
            return Ok(PrCi {
                state: MergeState::NotCreated,
                checks: CiStatus::Unknown,
                head_sha: String::new(),
            });
        }
        let out = self.run(
            dir,
            &[
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "state,headRefOid,statusCheckRollup",
            ],
        )?;
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
        let state = match v.get("state").and_then(|s| s.as_str()).unwrap_or("") {
            "MERGED" => MergeState::Merged,
            "OPEN" => MergeState::Open,
            "CLOSED" => MergeState::Closed,
            _ => MergeState::Unknown,
        };
        let head_sha = v
            .get("headRefOid")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let checks = match v.get("statusCheckRollup").and_then(|r| r.as_array()) {
            Some(items) => aggregate_checks(items),
            None => CiStatus::Unknown,
        };
        Ok(PrCi {
            state,
            checks,
            head_sha,
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

/// Folds the `statusCheckRollup` entries into one status. Each entry is either
/// a CheckRun (`status`/`conclusion`) or a StatusContext (`state`); failures
/// dominate, then pending, then unrecognized payloads.
fn aggregate_checks(items: &[serde_json::Value]) -> CiStatus {
    if items.is_empty() {
        return CiStatus::NoChecks;
    }
    let mut agg = CiStatus::Passing;
    for item in items {
        let one = classify_check(item);
        agg = match (agg, one) {
            (_, CiStatus::Failing) | (CiStatus::Failing, _) => CiStatus::Failing,
            (_, CiStatus::Pending) | (CiStatus::Pending, _) => CiStatus::Pending,
            (_, CiStatus::Unknown) | (CiStatus::Unknown, _) => CiStatus::Unknown,
            _ => CiStatus::Passing,
        };
    }
    agg
}

/// Status of a single rollup entry.
fn classify_check(item: &serde_json::Value) -> CiStatus {
    let get = |k: &str| item.get(k).and_then(|v| v.as_str()).unwrap_or("");
    // StatusContext (commit status API) carries `state` directly.
    if item.get("state").is_some() {
        return match get("state") {
            "SUCCESS" => CiStatus::Passing,
            "PENDING" | "EXPECTED" => CiStatus::Pending,
            "FAILURE" | "ERROR" => CiStatus::Failing,
            _ => CiStatus::Unknown,
        };
    }
    // CheckRun: anything not COMPLETED is still running/queued.
    if get("status") != "COMPLETED" {
        return CiStatus::Pending;
    }
    match get("conclusion") {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => CiStatus::Passing,
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            CiStatus::Failing
        }
        _ => CiStatus::Unknown,
    }
}

/// Extracts the PR number from the URL emitted by `gh pr create`. gh may print
/// warnings before the URL, so we use the last non-empty line.
fn parse_pr_number_from_url(s: &str) -> Option<u64> {
    let line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    line.trim().rsplit('/').next()?.parse().ok()
}
