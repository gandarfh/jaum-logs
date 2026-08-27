use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Gh;
use jaum_core::MergeState;

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
