use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
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

/// Escreve um `gh` falso e determinístico, exercitando construção de args e
/// parse de saída sem rede/auth. Convenções:
///   pr create            -> imprime URL com PR 142 (precedida de um aviso)
///   pr list (head normal)-> "7"; (head "missing/branch") -> ""
///   pr view <n>          -> 1=OPEN 2=MERGED 3=CLOSED 99=WEIRD
fn fake_gh(dir: &TmpDir) -> String {
    let path = dir.0.join("gh");
    let script = r#"#!/usr/bin/env bash
case "$1 $2" in
  "pr create")
    echo "Warning: using default base"
    echo "https://github.com/owner/repo/pull/142"
    ;;
  "pr list")
    if [ "$6" = "missing/branch" ]; then echo ""; else echo "7"; fi
    ;;
  "pr view")
    case "$3" in
      1) echo "OPEN" ;;
      2) echo "MERGED" ;;
      3) echo "CLOSED" ;;
      *) echo "WEIRD" ;;
    esac
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
fn pr_create_extrai_numero_da_url() {
    let dir = TmpDir::new("create");
    let gh = Gh::with_bin(fake_gh(&dir));
    let n = gh.pr_create("owner/repo", "feat/x").unwrap();
    assert_eq!(n, 142);
}

#[test]
fn pr_number_parseia_resultado() {
    let dir = TmpDir::new("number");
    let gh = Gh::with_bin(fake_gh(&dir));
    assert_eq!(gh.pr_number("owner/repo", "feat/x").unwrap(), 7);
}

#[test]
fn pr_number_zero_quando_nao_existe() {
    let dir = TmpDir::new("number-zero");
    let gh = Gh::with_bin(fake_gh(&dir));
    assert_eq!(gh.pr_number("owner/repo", "missing/branch").unwrap(), 0);
}

#[test]
fn pr_merge_state_mapeia_estados() {
    let dir = TmpDir::new("state");
    let gh = Gh::with_bin(fake_gh(&dir));
    assert_eq!(
        gh.pr_merge_state("owner/repo", 1).unwrap(),
        MergeState::Open
    );
    assert_eq!(
        gh.pr_merge_state("owner/repo", 2).unwrap(),
        MergeState::Merged
    );
    assert_eq!(
        gh.pr_merge_state("owner/repo", 3).unwrap(),
        MergeState::Closed
    );
    assert_eq!(
        gh.pr_merge_state("owner/repo", 99).unwrap(),
        MergeState::Unknown
    );
}

#[test]
fn pr_merge_state_zero_eh_not_created_sem_chamar_gh() {
    // bin inexistente: se chamasse o gh, falharia. pr==0 curto-circuita.
    let gh = Gh::with_bin("/nao/existe/gh");
    assert_eq!(
        gh.pr_merge_state("owner/repo", 0).unwrap(),
        MergeState::NotCreated
    );
}
