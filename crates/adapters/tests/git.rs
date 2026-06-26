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
    assert!(ok, "git {args:?} falhou");
}

/// Repo fixture com branch default `main` e um commit inicial.
fn init_repo(dir: &TmpDir) -> PathBuf {
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main", "-q"]);
    git(&repo, &["config", "user.email", "t@test.dev"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "linha um\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

#[test]
fn branch_create_eh_idempotente() {
    let dir = TmpDir::new("branch");
    let repo = init_repo(&dir);
    let g = Git::new();

    g.branch_create(&repo, "feat/x").unwrap();
    // segunda vez não deve falhar
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
fn worktree_add_cria_e_remove_apaga() {
    let dir = TmpDir::new("worktree");
    let repo = init_repo(&dir);
    let g = Git::new();

    let wt = g.worktree_add(&repo, "feat/task-012").unwrap();
    assert!(wt.exists(), "worktree não foi criada");
    assert!(wt.join("README.md").exists(), "checkout não trouxe arquivos");
    // o nome do diretório sanitiza a `/`
    assert_eq!(wt.file_name().unwrap().to_str().unwrap(), "feat-task-012");

    g.worktree_remove(&repo, "feat/task-012").unwrap();
    assert!(!wt.exists(), "worktree não foi removida");
}

#[test]
fn worktree_add_em_branch_existente_funciona() {
    let dir = TmpDir::new("worktree-existing");
    let repo = init_repo(&dir);
    let g = Git::new();

    g.branch_create(&repo, "feat/y").unwrap();
    let wt = g.worktree_add(&repo, "feat/y").unwrap();
    assert!(wt.exists());
}

#[test]
fn diff_mostra_mudancas_da_branch() {
    let dir = TmpDir::new("diff");
    let repo = init_repo(&dir);
    let g = Git::new();

    // cria branch + worktree, commita uma mudança lá; objetos são compartilhados
    let wt = g.worktree_add(&repo, "feat/change").unwrap();
    fs::write(wt.join("README.md"), "linha um\nlinha dois\n").unwrap();
    git(&wt, &["config", "user.email", "t@test.dev"]);
    git(&wt, &["config", "user.name", "Test"]);
    git(&wt, &["commit", "-aqm", "muda readme"]);

    let diff = g.diff(&repo, "feat/change").unwrap();
    assert!(diff.contains("linha dois"), "diff não refletiu a mudança:\n{diff}");
    assert!(diff.contains("README.md"));
}

#[test]
fn diff_em_branch_sem_mudanca_eh_vazio() {
    let dir = TmpDir::new("diff-empty");
    let repo = init_repo(&dir);
    let g = Git::new();
    g.branch_create(&repo, "feat/nada").unwrap();
    let diff = g.diff(&repo, "feat/nada").unwrap();
    assert!(diff.trim().is_empty());
}
