use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Adapter de `git` via shell-out. Opera sobre o diretório local de um
/// repositório (`repo`). O mapeamento slug "owner/name" -> caminho local é
/// responsabilidade de quem orquestra (play), não deste adapter.
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
        Self { bin: "git".to_string() }
    }

    /// Permite apontar para um binário alternativo (usado em testes).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Cria a branch a partir do HEAD atual. Idempotente: se já existe, no-op.
    pub fn branch_create(&self, repo: &Path, branch: &str) -> Result<()> {
        if self.branch_exists(repo, branch)? {
            return Ok(());
        }
        self.run(repo, &["branch", branch])?;
        Ok(())
    }

    /// Cria uma worktree para `branch` (criando a branch se ainda não existir).
    /// Devolve o caminho da worktree.
    pub fn worktree_add(&self, repo: &Path, branch: &str) -> Result<PathBuf> {
        let path = self.worktree_path(repo, branch);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("criando diretório de worktrees {}", parent.display()))?;
        }
        let path_str = path.to_string_lossy().into_owned();
        if self.branch_exists(repo, branch)? {
            self.run(repo, &["worktree", "add", &path_str, branch])?;
        } else {
            self.run(repo, &["worktree", "add", "-b", branch, &path_str])?;
        }
        Ok(path)
    }

    /// Remove a worktree de `branch`. Não usa `--force`: worktree suja falha,
    /// para não descartar trabalho silenciosamente.
    pub fn worktree_remove(&self, repo: &Path, branch: &str) -> Result<()> {
        let path = self.worktree_path(repo, branch);
        let path_str = path.to_string_lossy().into_owned();
        self.run(repo, &["worktree", "remove", &path_str])?;
        Ok(())
    }

    /// Diff de `branch` contra a branch default (three-dot: mudanças desde a
    /// divergência). É o diff do PR, usado pelo review.
    pub fn diff(&self, repo: &Path, branch: &str) -> Result<String> {
        let base = self.default_branch(repo)?;
        let spec = format!("{base}...{branch}");
        self.run(repo, &["diff", &spec])
    }

    // --- internos ----------------------------------------------------------

    fn branch_exists(&self, repo: &Path, branch: &str) -> Result<bool> {
        let out = Command::new(&self.bin)
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .with_context(|| format!("executando {} rev-parse", self.bin))?;
        Ok(out.status.success())
    }

    /// Detecta a branch default: `origin/HEAD`, senão `main`, senão `master`.
    fn default_branch(&self, repo: &Path) -> Result<String> {
        if let Ok(out) = self.run(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
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
        bail!("não foi possível detectar a branch default de {}", repo.display())
    }

    /// Caminho da worktree: irmão do repo, fora da árvore de trabalho principal.
    /// `feat/task-012` vira `feat-task-012` para ser um nome de diretório válido.
    fn worktree_path(&self, repo: &Path, branch: &str) -> PathBuf {
        let safe = branch.replace('/', "-");
        let name = repo
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        repo.with_file_name(format!("{name}.worktrees")).join(safe)
    }

    fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .with_context(|| format!("executando {} {args:?}", self.bin))?;
        if !out.status.success() {
            bail!(
                "{} {args:?} falhou: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
