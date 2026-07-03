use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Gh;
use jaum_core::{MergeState, Status, Store};
use jaum_flows::finish::Finish;

/// Repo map for Finish: each slug points to an existing directory (the fake
/// `gh` ignores the cwd, but `current_dir` requires a valid path).
fn repos(dir: &TmpDir) -> HashMap<String, PathBuf> {
    ["org/x", "org/y", "slyde"]
        .iter()
        .map(|s| (s.to_string(), dir.0.clone()))
        .collect()
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-finish-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fake `gh`:
///   pr list  (head feat/open -> "5", otherwise "0")
///   pr view  (7 -> MERGED, 5 -> OPEN, 9 -> CLOSED)
fn fake_gh(dir: &TmpDir) -> String {
    let path = dir.0.join("gh");
    let script = r#"#!/usr/bin/env bash
case "$1 $2" in
  "pr list") if [ "$4" = "feat/open" ]; then echo "5"; else echo "0"; fi ;;
  "pr view")
    case "$3" in
      7) echo "MERGED" ;;
      5) echo "OPEN" ;;
      9) echo "CLOSED" ;;
      *) echo "WEIRD" ;;
    esac ;;
esac
"#;
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

fn store_with(dir: &TmpDir, id: &str, status: &str, prs: &str) -> Store {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    let md =
        format!("---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n{prs}---\n\n## Objective\nx\n");
    fs::write(backlog.join(format!("{id}.md")), md).unwrap();
    Store::new(&backlog)
}

#[test]
fn run_marks_merged_when_all_prs_merged() {
    let dir = TmpDir::new("merged");
    let store = store_with(
        &dir,
        "TASK-001",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    let agg = finish.run("TASK-001").unwrap();
    assert_eq!(agg, MergeState::Merged);
    assert_eq!(store.get("TASK-001").unwrap().status, Status::Merged);
}

#[test]
fn run_does_not_merge_and_persists_discovered_number() {
    let dir = TmpDir::new("open");
    let store = store_with(
        &dir,
        "TASK-002",
        "review",
        "  - repo: org/x\n    pr: 0\n    branch: feat/open\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    let agg = finish.run("TASK-002").unwrap();
    assert_eq!(agg, MergeState::Open);
    // status did NOT become merged
    assert_eq!(store.get("TASK-002").unwrap().status, Status::Review);
    // discovered PR number was persisted
    assert_eq!(store.get("TASK-002").unwrap().prs[0].pr, 5);
}

#[test]
fn run_multi_pr_open_aggregates_open() {
    let dir = TmpDir::new("multi");
    let store = store_with(
        &dir,
        "TASK-003",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n  - repo: org/y\n    pr: 5\n    branch: feat/b\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    let agg = finish.run("TASK-003").unwrap();
    assert_eq!(agg, MergeState::Open); // one merged + one open
    assert_eq!(store.get("TASK-003").unwrap().status, Status::Review);
}

#[test]
fn run_tolerates_failing_gh_as_not_created() {
    // local project with no GitHub remote: `gh` fails (slug isn't owner/name,
    // no auth, etc). Finish should treat it as NotCreated, not propagate the error.
    let dir = TmpDir::new("nogh");
    let store = store_with(
        &dir,
        "TASK-009",
        "review",
        "  - repo: slyde\n    pr: 0\n    branch: feat/markdown-parser\n",
    );
    let gh = Gh::with_bin("false"); // exits with error on any call
    let finish = Finish::new(&store, &gh, repos(&dir));

    let agg = finish.run("TASK-009").unwrap();
    assert_eq!(agg, MergeState::NotCreated);
    assert_eq!(store.get("TASK-009").unwrap().status, Status::Review);
}

#[test]
fn run_treats_unmapped_repo_as_not_created() {
    let dir = TmpDir::new("unmapped");
    let store = store_with(
        &dir,
        "TASK-005",
        "review",
        "  - repo: org/unmapped\n    pr: 7\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir)); // org/unmapped absent

    let agg = finish.run("TASK-005").unwrap();
    assert_eq!(agg, MergeState::NotCreated);
    assert_eq!(store.get("TASK-005").unwrap().status, Status::Review);
}

#[test]
fn run_without_prs_is_not_created() {
    let dir = TmpDir::new("noprs");
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(
        backlog.join("TASK-006.md"),
        "---\nid: TASK-006\ntype: impl\nstatus: review\n---\n\n## Objective\nx\n",
    )
    .unwrap();
    let store = Store::new(&backlog);
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    assert_eq!(finish.run("TASK-006").unwrap(), MergeState::NotCreated);
}

#[test]
fn run_aggregates_closed_pr_as_closed() {
    let dir = TmpDir::new("closed");
    let store = store_with(
        &dir,
        "TASK-007",
        "review",
        "  - repo: org/x\n    pr: 9\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    assert_eq!(finish.run("TASK-007").unwrap(), MergeState::Closed);
    assert_eq!(store.get("TASK-007").unwrap().status, Status::Review);
}

#[test]
fn run_aggregates_unrecognized_gh_state_as_unknown() {
    let dir = TmpDir::new("unknown");
    // pr 8 hits the fake gh fallback ("WEIRD") -> MergeState::Unknown
    let store = store_with(
        &dir,
        "TASK-008",
        "review",
        "  - repo: org/x\n    pr: 8\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    assert_eq!(finish.run("TASK-008").unwrap(), MergeState::Unknown);
    assert_eq!(store.get("TASK-008").unwrap().status, Status::Review);
}

#[test]
fn merge_state_only_reads_does_not_change_status() {
    let dir = TmpDir::new("readonly");
    let store = store_with(
        &dir,
        "TASK-004",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh, repos(&dir));

    assert_eq!(finish.merge_state("TASK-004").unwrap(), MergeState::Merged);
    // even when Merged, merge_state does not change the status
    assert_eq!(store.get("TASK-004").unwrap().status, Status::Review);
}
