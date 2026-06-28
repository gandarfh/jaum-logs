//! Config global do jaum em `~/jaum/config.toml`: lista de projetos, cada um
//! com seu `.backlog/`, `docs/` e N repos (slug -> caminho local).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Um repo de um projeto: slug "owner/name" e caminho local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMap {
    pub slug: String,
    pub path: PathBuf,
}

/// Um projeto: nome + caminhos + N repos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub backlog: PathBuf,
    #[serde(default = "default_docs")]
    pub docs: PathBuf,
    #[serde(default = "default_work")]
    pub work_dir: PathBuf,
    #[serde(default)]
    pub repos: Vec<RepoMap>,
}

fn default_docs() -> PathBuf {
    PathBuf::from("docs")
}
fn default_work() -> PathBuf {
    PathBuf::from(".jaum")
}

impl Project {
    /// Mapa slug -> path para alimentar Play/Review.
    pub fn repo_map(&self) -> HashMap<String, PathBuf> {
        self.repos
            .iter()
            .map(|r| (r.slug.clone(), r.path.clone()))
            .collect()
    }
}

/// Config global: todos os projetos conhecidos.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl Config {
    /// Caminho do arquivo de config: `~/jaum/config.toml`.
    pub fn path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("variável HOME não definida")?;
        Ok(PathBuf::from(home).join("jaum").join("config.toml"))
    }

    /// Carrega a config (vazia se o arquivo ainda não existe).
    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parseando {}", path.display()))
    }

    /// Grava a config em `~/jaum/config.toml`.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("criando {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializando config")?;
        fs::write(&path, raw).with_context(|| format!("gravando {}", path.display()))
    }

    pub fn find(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Projeto cujo `.backlog/` corresponde ao diretório atual (canônico).
    pub fn project_for_cwd(&self, cwd: &Path) -> Option<&Project> {
        let cwd = fs::canonicalize(cwd).ok()?;
        self.projects.iter().find(|p| {
            fs::canonicalize(&p.backlog)
                .map(|b| b.parent().map(|d| d == cwd).unwrap_or(false))
                .unwrap_or(false)
        })
    }
}

/// Scaffolding: cria `.backlog/`, `docs/`, `.jaum/` no `root`, detecta repos e
/// registra (ou atualiza) o projeto na config global. Devolve o projeto criado.
pub fn init_project(root: &Path, explicit_repos: &[PathBuf]) -> Result<Project> {
    let root = fs::canonicalize(root).with_context(|| format!("resolvendo {}", root.display()))?;
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("projeto")
        .to_string();

    for sub in [".backlog", "docs", ".jaum"] {
        fs::create_dir_all(root.join(sub)).with_context(|| format!("criando {sub}/"))?;
    }

    let repo_dirs = if explicit_repos.is_empty() {
        detect_repos(&root)
    } else {
        explicit_repos
            .iter()
            .filter_map(|p| fs::canonicalize(p).ok())
            .collect()
    };
    let repos: Vec<RepoMap> = repo_dirs
        .into_iter()
        .map(|path| RepoMap {
            slug: repo_slug(&path),
            path,
        })
        .collect();

    let project = Project {
        name: name.clone(),
        backlog: root.join(".backlog"),
        docs: root.join("docs"),
        work_dir: root.join(".jaum"),
        repos,
    };

    let mut cfg = Config::load()?;
    if cfg.find(&name).is_some() {
        // substitui a entrada de mesmo nome
        cfg.projects.retain(|p| p.name != name);
    }
    cfg.projects.push(project.clone());
    cfg.save()?;
    Ok(project)
}

/// Detecta repos git DENTRO do projeto: o próprio `root` e seus subdiretórios
/// diretos. NÃO varre irmãos (a pasta-pai), para não puxar projetos vizinhos.
/// Ignora worktrees (onde `.git` é um arquivo, não um diretório).
fn detect_repos(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut consider = |dir: &Path| {
        // só clones de verdade: `.git` é um diretório (worktree tem `.git` arquivo)
        if dir.join(".git").is_dir()
            && let Ok(canon) = fs::canonicalize(dir)
            && !found.contains(&canon)
        {
            found.push(canon);
        }
    };
    consider(root);
    if let Ok(rd) = fs::read_dir(root) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                consider(&p);
            }
        }
    }
    found
}

/// Slug "owner/name" pelo remote origin; fallback para o nome da pasta.
fn repo_slug(repo: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .output()
        && out.status.success()
    {
        let url = String::from_utf8_lossy(&out.stdout);
        if let Some(slug) = slug_from_url(url.trim()) {
            return slug;
        }
    }
    repo.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string()
}

/// Extrai "owner/name" de uma URL git (ssh ou https).
pub(crate) fn slug_from_url(url: &str) -> Option<String> {
    let s = url.trim_end_matches(".git");
    // git@host:owner/name  ou  https://host/owner/name
    let tail = s.rsplit(':').next()?;
    let parts: Vec<&str> = tail.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        return Some(format!("{}/{}", parts[1], parts[0]));
    }
    None
}

/// Garante que exista ao menos um projeto utilizável; senão, instrui o init.
pub fn ensure_usable(cfg: &Config) -> Result<()> {
    if cfg.projects.is_empty() {
        bail!("nenhum projeto registrado. Rode `jaum init` na raiz de um projeto.");
    }
    Ok(())
}
