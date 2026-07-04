use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Gh;
use jaum_core::{CiStatus, MergeState};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-gh-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes a fake, deterministic `gh` exercising arg construction and output
/// parsing without network/auth. Conventions:
///   pr create            -> prints URL with PR 142 (preceded by a warning)
///   pr list (normal head)-> "7"; (head "missing/branch") -> ""
///   pr view <n>          -> 1=OPEN 2=MERGED 3=CLOSED 99=WEIRD
///   pr view <n> (ci)     -> 10=green 11=pending 12=failing 13=no checks
///                           14=unknown 15=bad json 16=failing(statuses)
///                           17=unknown(status) 18=null rollup 501=gh error
/// Written once per process: a per-test copy races on Linux, where a fork
/// from a parallel test can inherit the still-open write fd and make the
/// exec fail with ETXTBSY.
fn fake_gh() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(write_fake_gh).clone()
}

fn write_fake_gh() -> String {
    let mut dir = std::env::temp_dir();
    dir.push(format!("jaum-gh-bin-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gh");
    let script = r#"#!/usr/bin/env bash
case "$1 $2" in
  "pr create")
    echo "Warning: using default base"
    echo "https://github.com/owner/repo/pull/142"
    ;;
  "pr list")
    if [ "$4" = "missing/branch" ]; then echo ""; else echo "7"; fi
    ;;
  "pr diff")
    if [ "$3" = "500" ]; then echo "boom" >&2; exit 1; fi
    echo "diff --git a/f b/f"
    ;;
  "pr view")
    if [ "$5" = "title,body" ]; then
      if [ "$3" = "98" ]; then echo "not json"; else echo '{"title":"T","body":"B"}'; fi
    elif [ "$5" = "state,headRefOid,statusCheckRollup" ]; then
      case "$3" in
        10) echo '{"state":"OPEN","headRefOid":"aaa111","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"COMPLETED","conclusion":"NEUTRAL"},{"status":"COMPLETED","conclusion":"SKIPPED"},{"state":"SUCCESS"}]}' ;;
        11) echo '{"state":"OPEN","headRefOid":"bbb222","statusCheckRollup":[{"status":"IN_PROGRESS","conclusion":""},{"state":"PENDING"},{"state":"EXPECTED"},{"status":"COMPLETED","conclusion":"SUCCESS"}]}' ;;
        12) echo '{"state":"OPEN","headRefOid":"ccc333","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"},{"status":"QUEUED","conclusion":""},{"state":"SUCCESS"}]}' ;;
        13) echo '{"state":"OPEN","headRefOid":"ddd444","statusCheckRollup":[]}' ;;
        14) echo '{"state":"WEIRD","headRefOid":"eee555","statusCheckRollup":[{"status":"COMPLETED","conclusion":"WEIRD"}]}' ;;
        15) echo 'not json' ;;
        16) echo '{"state":"MERGED","headRefOid":"fff666","statusCheckRollup":[{"state":"ERROR"},{"status":"COMPLETED","conclusion":"TIMED_OUT"},{"status":"COMPLETED","conclusion":"CANCELLED"},{"status":"COMPLETED","conclusion":"ACTION_REQUIRED"},{"status":"COMPLETED","conclusion":"STARTUP_FAILURE"},{"state":"FAILURE"}]}' ;;
        17) echo '{"state":"CLOSED","headRefOid":"ggg777","statusCheckRollup":[{"state":"WEIRD"},{"status":"COMPLETED","conclusion":"SUCCESS"}]}' ;;
        18) echo '{"state":"OPEN","headRefOid":"hhh888","statusCheckRollup":null}' ;;
        501) echo "no checks reported" >&2; exit 1 ;;
      esac
    else
      case "$3" in
        1) echo "OPEN" ;;
        2) echo "MERGED" ;;
        3) echo "CLOSED" ;;
        *) echo "WEIRD" ;;
      esac
    fi
    ;;
esac
"#;
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn pr_create_extracts_number_from_url() {
    let dir = TmpDir::new("create");
    let gh = Gh::with_bin(fake_gh());
    let n = gh.pr_create(&dir.0, "feat/x").unwrap();
    assert_eq!(n, 142);
}

#[test]
fn pr_number_parses_result() {
    let dir = TmpDir::new("number");
    let gh = Gh::with_bin(fake_gh());
    assert_eq!(gh.pr_number(&dir.0, "feat/x").unwrap(), 7);
}

#[test]
fn pr_number_zero_when_none_exists() {
    let dir = TmpDir::new("number-zero");
    let gh = Gh::with_bin(fake_gh());
    assert_eq!(gh.pr_number(&dir.0, "missing/branch").unwrap(), 0);
}

#[test]
fn pr_merge_state_maps_states() {
    let dir = TmpDir::new("state");
    let gh = Gh::with_bin(fake_gh());
    assert_eq!(gh.pr_merge_state(&dir.0, 1).unwrap(), MergeState::Open);
    assert_eq!(gh.pr_merge_state(&dir.0, 2).unwrap(), MergeState::Merged);
    assert_eq!(gh.pr_merge_state(&dir.0, 3).unwrap(), MergeState::Closed);
    assert_eq!(gh.pr_merge_state(&dir.0, 99).unwrap(), MergeState::Unknown);
}

#[test]
fn default_uses_the_real_gh_binary_name() {
    // construction only; nothing is executed.
    let _ = Gh::default();
}

#[test]
fn pr_diff_zero_is_empty_without_calling_gh() {
    let gh = Gh::with_bin("/does/not/exist/gh");
    assert_eq!(gh.pr_diff(std::path::Path::new("."), 0).unwrap(), "");
}

#[test]
fn pr_diff_returns_gh_output() {
    let dir = TmpDir::new("diff");
    let gh = Gh::with_bin(fake_gh());
    let diff = gh.pr_diff(&dir.0, 7).unwrap();
    assert!(diff.contains("diff --git"));
}

#[test]
fn pr_diff_propagates_gh_failure() {
    let dir = TmpDir::new("diff-fail");
    let gh = Gh::with_bin(fake_gh());
    let err = gh.pr_diff(&dir.0, 500).unwrap_err();
    assert!(err.to_string().contains("boom"));
}

#[test]
fn pr_view_zero_is_empty_without_calling_gh() {
    let gh = Gh::with_bin("/does/not/exist/gh");
    let (title, body) = gh.pr_view(std::path::Path::new("."), 0).unwrap();
    assert_eq!(title, "");
    assert_eq!(body, "");
}

#[test]
fn pr_view_parses_title_and_body() {
    let dir = TmpDir::new("view");
    let gh = Gh::with_bin(fake_gh());
    let (title, body) = gh.pr_view(&dir.0, 7).unwrap();
    assert_eq!(title, "T");
    assert_eq!(body, "B");
}

#[test]
fn pr_view_tolerates_malformed_json() {
    let dir = TmpDir::new("view-bad");
    let gh = Gh::with_bin(fake_gh());
    let (title, body) = gh.pr_view(&dir.0, 98).unwrap();
    assert_eq!(title, "");
    assert_eq!(body, "");
}

#[test]
fn pr_ci_zero_short_circuits_without_calling_gh() {
    let gh = Gh::with_bin("/does/not/exist/gh");
    let ci = gh.pr_ci(std::path::Path::new("."), 0).unwrap();
    assert_eq!(ci.state, MergeState::NotCreated);
    assert_eq!(ci.checks, CiStatus::Unknown);
    assert_eq!(ci.head_sha, "");
}

#[test]
fn pr_ci_reads_state_head_and_passing_checks() {
    let dir = TmpDir::new("ci-green");
    let gh = Gh::with_bin(fake_gh());
    let ci = gh.pr_ci(&dir.0, 10).unwrap();
    assert_eq!(ci.state, MergeState::Open);
    assert_eq!(ci.checks, CiStatus::Passing);
    assert_eq!(ci.head_sha, "aaa111");
}

#[test]
fn pr_ci_pending_wins_over_passing() {
    let dir = TmpDir::new("ci-pending");
    let gh = Gh::with_bin(fake_gh());
    let ci = gh.pr_ci(&dir.0, 11).unwrap();
    assert_eq!(ci.checks, CiStatus::Pending);
}

#[test]
fn pr_ci_failure_wins_over_pending_and_passing() {
    let dir = TmpDir::new("ci-fail");
    let gh = Gh::with_bin(fake_gh());
    let ci = gh.pr_ci(&dir.0, 12).unwrap();
    assert_eq!(ci.checks, CiStatus::Failing);
    // failing status contexts and check-run conclusions also map to Failing
    let ci = gh.pr_ci(&dir.0, 16).unwrap();
    assert_eq!(ci.state, MergeState::Merged);
    assert_eq!(ci.checks, CiStatus::Failing);
}

#[test]
fn pr_ci_distinguishes_pr_without_checks() {
    let dir = TmpDir::new("ci-none");
    let gh = Gh::with_bin(fake_gh());
    let ci = gh.pr_ci(&dir.0, 13).unwrap();
    assert_eq!(ci.checks, CiStatus::NoChecks);
    assert_eq!(ci.head_sha, "ddd444");
}

#[test]
fn pr_ci_unrecognized_payloads_are_unknown() {
    let dir = TmpDir::new("ci-unknown");
    let gh = Gh::with_bin(fake_gh());
    // unknown check-run conclusion + unknown PR state
    let ci = gh.pr_ci(&dir.0, 14).unwrap();
    assert_eq!(ci.state, MergeState::Unknown);
    assert_eq!(ci.checks, CiStatus::Unknown);
    // unknown status-context state does not become Passing
    let ci = gh.pr_ci(&dir.0, 17).unwrap();
    assert_eq!(ci.state, MergeState::Closed);
    assert_eq!(ci.checks, CiStatus::Unknown);
    // rollup that is not an array
    let ci = gh.pr_ci(&dir.0, 18).unwrap();
    assert_eq!(ci.checks, CiStatus::Unknown);
    assert_eq!(ci.head_sha, "hhh888");
}

#[test]
fn pr_ci_tolerates_malformed_json() {
    let dir = TmpDir::new("ci-badjson");
    let gh = Gh::with_bin(fake_gh());
    let ci = gh.pr_ci(&dir.0, 15).unwrap();
    assert_eq!(ci.state, MergeState::Unknown);
    assert_eq!(ci.checks, CiStatus::Unknown);
    assert_eq!(ci.head_sha, "");
}

#[test]
fn pr_ci_propagates_gh_failure() {
    let dir = TmpDir::new("ci-err");
    let gh = Gh::with_bin(fake_gh());
    let err = gh.pr_ci(&dir.0, 501).unwrap_err();
    assert!(err.to_string().contains("no checks reported"));
}

#[test]
fn missing_binary_errors_with_context() {
    let dir = TmpDir::new("nobin");
    let gh = Gh::with_bin("/does/not/exist/gh");
    let err = gh.pr_number(&dir.0, "feat/x").unwrap_err();
    assert!(err.to_string().contains("running"));
}

#[test]
fn pr_merge_state_zero_is_not_created_without_calling_gh() {
    // nonexistent bin: if it called gh, it would fail. pr==0 short-circuits.
    let gh = Gh::with_bin("/does/not/exist/gh");
    // pr==0 short-circuits before calling gh; the path isn't even used.
    assert_eq!(
        gh.pr_merge_state(std::path::Path::new("."), 0).unwrap(),
        MergeState::NotCreated
    );
}
