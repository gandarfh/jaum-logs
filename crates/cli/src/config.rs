//! Global jaum config in `~/jaum/config.toml`: list of projects, each with its
//! own `.backlog/`, `docs/` and N repos (slug -> local path).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A project's repo: "owner/name" slug and local path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMap {
    pub slug: String,
    pub path: PathBuf,
}

/// A project. Everything jaum owns is EXTERNAL (`~/jaum/<name>/...`); `root` is the
/// project folder (code only), used to recognize the cwd and detect repos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    /// Project folder (where `jaum init` ran). jaum never writes here.
    #[serde(default)]
    pub root: PathBuf,
    pub backlog: PathBuf,
    #[serde(default)]
    pub docs: PathBuf,
    #[serde(default)]
    pub work_dir: PathBuf,
    #[serde(default)]
    pub repos: Vec<RepoMap>,
}

impl Project {
    /// slug -> path map to feed Play/Review.
    pub fn repo_map(&self) -> HashMap<String, PathBuf> {
        self.repos
            .iter()
            .map(|r| (r.slug.clone(), r.path.clone()))
            .collect()
    }

    /// Project's external directory in jaum (`~/jaum/<name>`).
    pub fn home(&self) -> PathBuf {
        self.backlog
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.backlog.clone())
    }

    /// Path of `conventions.md` (project conventions, external).
    pub fn conventions_path(&self) -> PathBuf {
        self.home().join("conventions.md")
    }
}

/// Initial `conventions.md` template.
pub const CONVENTIONS_TEMPLATE: &str = "# Project conventions\n\nGuidelines injected into every play session and checked at review.\nOne per line (use `-`). Edit in the TUI (`e`) or capture on the fly (`c`).\n\n- \n";

/// Global config: all known projects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub projects: Vec<Project>,
}

/// jaum base directory: `~/jaum`.
pub fn jaum_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME variable not set")?;
    Ok(PathBuf::from(home).join("jaum"))
}

impl Config {
    /// Config file path: `~/jaum/config.toml`.
    pub fn path() -> Result<PathBuf> {
        Ok(jaum_home()?.join("config.toml"))
    }

    /// Load the config (empty if the file does not exist yet).
    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the config to `~/jaum/config.toml`.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }

    pub fn find(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Project matching the current directory: by `root` (external layout);
    /// falls back to the old layout (backlog inside the project).
    pub fn project_for_cwd(&self, cwd: &Path) -> Option<&Project> {
        let cwd = fs::canonicalize(cwd).ok()?;
        self.projects.iter().find(|p| {
            if fs::canonicalize(&p.root).map(|r| r == cwd).unwrap_or(false) {
                return true;
            }
            fs::canonicalize(&p.backlog)
                .map(|b| b.parent().map(|d| d == cwd).unwrap_or(false))
                .unwrap_or(false)
        })
    }
}

/// EXTERNAL scaffolding: creates `~/jaum/<name>/{docs,backlog,work}`, detects the
/// repos inside `root` and registers the project. Writes NOTHING under `root`.
pub fn init_project(root: &Path, explicit_repos: &[PathBuf]) -> Result<Project> {
    let root = fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let home = jaum_home()?.join(&name);
    for sub in ["docs", "backlog", "work"] {
        fs::create_dir_all(home.join(sub))
            .with_context(|| format!("creating {}", home.join(sub).display()))?;
    }
    // conventions.md (project conventions) — do not overwrite if it exists
    let conv = home.join("conventions.md");
    if !conv.exists() {
        fs::write(&conv, CONVENTIONS_TEMPLATE)
            .with_context(|| format!("creating {}", conv.display()))?;
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
        root,
        backlog: home.join("backlog"),
        docs: home.join("docs"),
        work_dir: home.join("work"),
        repos,
    };

    let mut cfg = Config::load()?;
    if cfg.find(&name).is_some() {
        // replace the entry with the same name
        cfg.projects.retain(|p| p.name != name);
    }
    cfg.projects.push(project.clone());
    cfg.save()?;
    Ok(project)
}

/// Detects git repos INSIDE the project: `root` itself and its direct
/// subdirectories. Does NOT scan siblings (the parent folder), to avoid pulling in
/// neighboring projects. Ignores worktrees (where `.git` is a file, not a directory).
fn detect_repos(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut consider = |dir: &Path| {
        // real clones only: `.git` is a directory (a worktree has a `.git` file)
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

/// "owner/name" slug from the origin remote; falls back to the folder name.
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

/// Extracts "owner/name" from a git URL (ssh or https).
pub(crate) fn slug_from_url(url: &str) -> Option<String> {
    let s = url.trim_end_matches(".git");
    // git@host:owner/name  or  https://host/owner/name
    let tail = s.rsplit(':').next()?;
    let parts: Vec<&str> = tail.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        return Some(format!("{}/{}", parts[1], parts[0]));
    }
    None
}

/// Ensures at least one usable project exists; otherwise points to init.
pub fn ensure_usable(cfg: &Config) -> Result<()> {
    if cfg.projects.is_empty() {
        bail!("no project registered. Run `jaum init` at a project root.");
    }
    Ok(())
}
