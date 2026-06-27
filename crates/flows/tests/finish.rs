use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::Gh;
use jaum_core::{MergeState, Status, Store};
use jaum_flows::finish::Finish;

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
///   pr list  (head feat/open -> "5", senão "0")
///   pr view  (7 -> MERGED, 5 -> OPEN, 9 -> CLOSED)
fn fake_gh(dir: &TmpDir) -> String {
    let path = dir.0.join("gh");
    let script = r#"#!/usr/bin/env bash
case "$1 $2" in
  "pr list") if [ "$6" = "feat/open" ]; then echo "5"; else echo "0"; fi ;;
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
    let md = format!("---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n{prs}---\n\n## Objetivo\nx\n");
    fs::write(backlog.join(format!("{id}.md")), md).unwrap();
    Store::new(&backlog)
}

#[test]
fn run_marca_merged_quando_todos_pr_mergeados() {
    let dir = TmpDir::new("merged");
    let store = store_with(
        &dir,
        "TASK-001",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh);

    let agg = finish.run("TASK-001").unwrap();
    assert_eq!(agg, MergeState::Merged);
    assert_eq!(store.get("TASK-001").unwrap().status, Status::Merged);
}

#[test]
fn run_nao_mergeia_e_persiste_numero_descoberto() {
    let dir = TmpDir::new("open");
    let store = store_with(
        &dir,
        "TASK-002",
        "review",
        "  - repo: org/x\n    pr: 0\n    branch: feat/open\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh);

    let agg = finish.run("TASK-002").unwrap();
    assert_eq!(agg, MergeState::Open);
    // status NÃO virou merged
    assert_eq!(store.get("TASK-002").unwrap().status, Status::Review);
    // número de PR descoberto foi persistido
    assert_eq!(store.get("TASK-002").unwrap().prs[0].pr, 5);
}

#[test]
fn run_multi_pr_aberto_agrega_open() {
    let dir = TmpDir::new("multi");
    let store = store_with(
        &dir,
        "TASK-003",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n  - repo: org/y\n    pr: 5\n    branch: feat/b\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh);

    let agg = finish.run("TASK-003").unwrap();
    assert_eq!(agg, MergeState::Open); // um merged + um aberto
    assert_eq!(store.get("TASK-003").unwrap().status, Status::Review);
}

#[test]
fn merge_state_so_le_nao_muda_status() {
    let dir = TmpDir::new("readonly");
    let store = store_with(
        &dir,
        "TASK-004",
        "review",
        "  - repo: org/x\n    pr: 7\n    branch: feat/a\n",
    );
    let gh = Gh::with_bin(fake_gh(&dir));
    let finish = Finish::new(&store, &gh);

    assert_eq!(finish.merge_state("TASK-004").unwrap(), MergeState::Merged);
    // mesmo Merged, merge_state não altera o status
    assert_eq!(store.get("TASK-004").unwrap().status, Status::Review);
}
