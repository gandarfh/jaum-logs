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

    /// Resolves the repo slug for `task new --branch` linking. An explicit
    /// slug must match a configured repo; otherwise falls back to the single
    /// configured repo, and is ambiguous (errors) with zero or 2+ repos.
    pub fn resolve_repo_slug(&self, explicit: Option<&str>) -> Result<String, String> {
        if let Some(slug) = explicit {
            return self
                .repos
                .iter()
                .find(|r| r.slug == slug)
                .map(|r| r.slug.clone())
                .ok_or_else(|| {
                    format!(
                        "unknown --repo '{slug}': configured repos are [{}]",
                        self.repos
                            .iter()
                            .map(|r| r.slug.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                });
        }
        match self.repos.as_slice() {
            [only] => Ok(only.slug.clone()),
            [] => Err("--branch given without --repo, and no repos are configured for this project; pass --repo explicitly".to_string()),
            many => Err(format!(
                "--branch given without --repo, and multiple repos are configured ([{}]); pass --repo explicitly",
                many.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

/// Initial `conventions.md` template.
pub const CONVENTIONS_TEMPLATE: &str = "# Project conventions\n\nGuidelines injected into every play session and checked at review.\nOne per line (use `-`). Edit in the TUI (`e`) or capture on the fly (`c`).\n\n- \n";

/// Global config: all known projects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub projects: Vec<Project>,
    /// Interval (seconds) between CI polls of the tasks' PRs; `None` = default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_poll_secs: Option<u64>,
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
        Self::load_from(&Self::path()?)
    }

    pub(crate) fn load_from(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the config to the given path (canonically `~/jaum/config.toml`).
    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
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
    init_project_in(&jaum_home()?, root, explicit_repos)
}

pub(crate) fn init_project_in(
    jaum_base: &Path,
    root: &Path,
    explicit_repos: &[PathBuf],
) -> Result<Project> {
    let root = fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let home = jaum_base.join(&name);
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

    let cfg_path = jaum_base.join("config.toml");
    let mut cfg = Config::load_from(&cfg_path)?;
    if cfg.find(&name).is_some() {
        // replace the entry with the same name
        cfg.projects.retain(|p| p.name != name);
    }
    cfg.projects.push(project.clone());
    cfg.save_to(&cfg_path)?;
    Ok(project)
}

/// Detects git repos INSIDE the project: `root` itself and its direct
/// subdirectories. Does NOT scan siblings (the parent folder), to avoid pulling in
/// neighboring projects. Ignores worktrees (where `.git` is a file, not a directory).
pub(crate) fn detect_repos(root: &Path) -> Vec<PathBuf> {
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
pub(crate) fn repo_slug(repo: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "jaum-cfg-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }

    fn project_at(root: PathBuf, backlog: PathBuf) -> Project {
        Project {
            name: "p".into(),
            root,
            backlog,
            docs: PathBuf::from("/none/docs"),
            work_dir: PathBuf::from("/none/work"),
            repos: Vec::new(),
        }
    }

    #[test]
    fn slug_from_url_handles_ssh_https_and_garbage() {
        assert_eq!(
            slug_from_url("git@github.com:acme/parser.git").as_deref(),
            Some("acme/parser")
        );
        assert_eq!(
            slug_from_url("https://github.com/acme/runtime").as_deref(),
            Some("acme/runtime")
        );
        assert_eq!(
            slug_from_url("https://github.com/acme/runtime.git").as_deref(),
            Some("acme/runtime")
        );
        assert_eq!(slug_from_url("git@host:justname"), None);
    }

    #[test]
    fn repo_map_builds_slug_to_path() {
        let mut p = project_at(PathBuf::from("/p"), PathBuf::from("/p/.backlog"));
        p.repos = vec![
            RepoMap {
                slug: "org/a".into(),
                path: PathBuf::from("/repos/a"),
            },
            RepoMap {
                slug: "org/b".into(),
                path: PathBuf::from("/repos/b"),
            },
        ];
        let map = p.repo_map();
        assert_eq!(map.get("org/a"), Some(&PathBuf::from("/repos/a")));
        assert_eq!(map.get("org/b"), Some(&PathBuf::from("/repos/b")));
    }

    #[test]
    fn project_home_and_conventions_path() {
        let p = project_at(PathBuf::from("/code/p"), PathBuf::from("/jaum/p/backlog"));
        assert_eq!(p.home(), PathBuf::from("/jaum/p"));
        assert_eq!(
            p.conventions_path(),
            PathBuf::from("/jaum/p/conventions.md")
        );

        // a backlog with no parent falls back to itself
        let rootless = project_at(PathBuf::from("/code/p"), PathBuf::from("/"));
        assert_eq!(rootless.home(), PathBuf::from("/"));
    }

    #[test]
    fn jaum_home_and_config_path_derive_from_home() {
        let home = jaum_home().unwrap();
        assert!(home.ends_with("jaum"));
        assert_eq!(Config::path().unwrap(), home.join("config.toml"));
    }

    #[test]
    fn load_from_missing_file_is_empty_config() {
        let dir = TmpDir::new("load-missing");
        let cfg = Config::load_from(&dir.path().join("config.toml")).unwrap();
        assert!(cfg.projects.is_empty());
    }

    #[test]
    fn save_to_then_load_from_roundtrips_and_creates_parents() {
        let dir = TmpDir::new("roundtrip");
        let path = dir.path().join("nested").join("config.toml");
        let cfg = Config {
            ci_poll_secs: None,
            projects: vec![project_at(
                dir.path().to_path_buf(),
                dir.path().join("backlog"),
            )],
        };
        cfg.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.projects, cfg.projects);
        assert_eq!(back.find("p").map(|p| p.name.as_str()), Some("p"));
        assert!(back.find("nope").is_none());

        // an empty path has no parent to create and cannot be written
        assert!(cfg.save_to(Path::new("")).is_err());
    }

    #[test]
    fn load_from_malformed_toml_fails_with_context() {
        let dir = TmpDir::new("malformed");
        let path = dir.path().join("config.toml");
        fs::write(&path, "not = [valid").unwrap();
        let err = Config::load_from(&path).unwrap_err();
        assert!(format!("{err:#}").contains("parsing"));
    }

    #[test]
    fn project_for_cwd_matches_by_root() {
        let dir = TmpDir::new("cwd-root");
        let cfg = Config {
            ci_poll_secs: None,
            projects: vec![project_at(
                dir.path().to_path_buf(),
                PathBuf::from("/none/backlog"),
            )],
        };
        assert!(cfg.project_for_cwd(dir.path()).is_some());
    }

    #[test]
    fn project_for_cwd_matches_old_layout_by_backlog_parent() {
        let dir = TmpDir::new("cwd-old");
        let backlog = dir.path().join(".backlog");
        fs::create_dir_all(&backlog).unwrap();
        let cfg = Config {
            ci_poll_secs: None,
            projects: vec![project_at(PathBuf::from("/none/root"), backlog)],
        };
        assert!(cfg.project_for_cwd(dir.path()).is_some());
    }

    #[test]
    fn project_for_cwd_misses_and_handles_bad_cwd() {
        let dir = TmpDir::new("cwd-miss");
        let other = TmpDir::new("cwd-other");
        let cfg = Config {
            ci_poll_secs: None,
            projects: vec![project_at(
                other.path().to_path_buf(),
                other.path().join(".backlog"),
            )],
        };
        assert!(cfg.project_for_cwd(dir.path()).is_none());
        assert!(
            cfg.project_for_cwd(&dir.path().join("does-not-exist"))
                .is_none()
        );
    }

    #[test]
    fn ensure_usable_requires_a_project() {
        let err = ensure_usable(&Config::default()).unwrap_err();
        assert!(err.to_string().contains("no project registered"));

        let dir = TmpDir::new("usable");
        let ok = Config {
            ci_poll_secs: None,
            projects: vec![project_at(
                dir.path().to_path_buf(),
                dir.path().join(".backlog"),
            )],
        };
        ensure_usable(&ok).unwrap();
    }

    #[test]
    fn detect_repos_finds_root_and_children_ignoring_worktrees() {
        let dir = TmpDir::new("detect");
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join("sub1/.git")).unwrap();
        fs::create_dir_all(dir.path().join("wt")).unwrap();
        fs::write(dir.path().join("wt/.git"), "gitdir: elsewhere").unwrap();
        fs::create_dir_all(dir.path().join("plain")).unwrap();
        fs::write(dir.path().join("file.txt"), "x").unwrap();
        // symlink resolving to the root: canonical dedup must skip it
        std::os::unix::fs::symlink(dir.path(), dir.path().join("selfref")).unwrap();

        let found = detect_repos(dir.path());
        let root = fs::canonicalize(dir.path()).unwrap();
        assert_eq!(found[0], root);
        assert!(found.contains(&root.join("sub1")));
        assert_eq!(found.len(), 2, "worktree/plain/symlink must be ignored");

        // a non-directory root has no repos (read_dir fails)
        assert!(detect_repos(&dir.path().join("file.txt")).is_empty());
    }

    #[test]
    fn repo_slug_uses_origin_and_falls_back_to_dir_name() {
        let dir = TmpDir::new("slug");
        let repo = dir.path().join("widget");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", "git@github.com:acme/widget.git"],
        );
        assert_eq!(repo_slug(&repo), "acme/widget");

        let bare = dir.path().join("noremote");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q"]);
        assert_eq!(repo_slug(&bare), "noremote");

        let plain = dir.path().join("plaindir");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(repo_slug(&plain), "plaindir");

        // origin exists but its URL has no owner/name shape
        let odd = dir.path().join("oddremote");
        fs::create_dir_all(&odd).unwrap();
        git(&odd, &["init", "-q"]);
        git(&odd, &["remote", "add", "origin", "justaname"]);
        assert_eq!(repo_slug(&odd), "oddremote");

        assert_eq!(repo_slug(Path::new("/")), "repo");
    }

    #[test]
    fn init_project_in_scaffolds_detects_and_registers() {
        let dir = TmpDir::new("init");
        let base = dir.path().join("jaumhome");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/proj.git",
            ],
        );

        let project = init_project_in(&base, &root, &[]).unwrap();
        assert_eq!(project.name, "proj");
        assert_eq!(project.root, fs::canonicalize(&root).unwrap());
        for sub in ["docs", "backlog", "work"] {
            assert!(base.join("proj").join(sub).is_dir());
        }
        let conv = base.join("proj/conventions.md");
        assert_eq!(fs::read_to_string(&conv).unwrap(), CONVENTIONS_TEMPLATE);
        assert_eq!(project.repos.len(), 1);
        assert_eq!(project.repos[0].slug, "acme/proj");

        let cfg = Config::load_from(&base.join("config.toml")).unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.find("proj").unwrap().backlog, base.join("proj/backlog"));

        // re-init: replaces the entry and keeps an edited conventions.md
        fs::write(&conv, "- keep me\n").unwrap();
        init_project_in(&base, &root, &[]).unwrap();
        assert_eq!(fs::read_to_string(&conv).unwrap(), "- keep me\n");
        let cfg = Config::load_from(&base.join("config.toml")).unwrap();
        assert_eq!(cfg.projects.len(), 1, "same name must be replaced");
    }

    #[test]
    fn init_project_in_uses_explicit_repos_dropping_missing_ones() {
        let dir = TmpDir::new("init-explicit");
        let base = dir.path().join("jaumhome");
        let root = dir.path().join("proj");
        let repo = dir.path().join("repo-a");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&repo).unwrap();

        let project = init_project_in(
            &base,
            &root,
            &[repo.clone(), dir.path().join("missing-repo")],
        )
        .unwrap();
        assert_eq!(project.repos.len(), 1);
        assert_eq!(project.repos[0].slug, "repo-a");
        assert_eq!(project.repos[0].path, fs::canonicalize(&repo).unwrap());
    }

    #[test]
    fn init_project_in_fails_on_missing_root() {
        let dir = TmpDir::new("init-bad");
        let err =
            init_project_in(&dir.path().join("base"), &dir.path().join("nope"), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("resolving"));
    }

    fn project_with_repos(slugs: &[&str]) -> Project {
        let mut p = project_at(PathBuf::from("/none/root"), PathBuf::from("/none/backlog"));
        p.repos = slugs
            .iter()
            .map(|s| RepoMap {
                slug: s.to_string(),
                path: PathBuf::from(format!("/none/{s}")),
            })
            .collect();
        p
    }

    #[test]
    fn resolve_repo_slug_explicit_match() {
        let p = project_with_repos(&["org/a", "org/b"]);
        assert_eq!(p.resolve_repo_slug(Some("org/b")).unwrap(), "org/b");
    }

    #[test]
    fn resolve_repo_slug_explicit_unknown() {
        let p = project_with_repos(&["org/a"]);
        let err = p.resolve_repo_slug(Some("org/other")).unwrap_err();
        assert!(err.contains("unknown --repo 'org/other'"));
        assert!(err.contains("org/a"));
    }

    #[test]
    fn resolve_repo_slug_auto_resolves_single_repo() {
        let p = project_with_repos(&["org/only"]);
        assert_eq!(p.resolve_repo_slug(None).unwrap(), "org/only");
    }

    #[test]
    fn resolve_repo_slug_ambiguous_with_no_repos() {
        let p = project_with_repos(&[]);
        let err = p.resolve_repo_slug(None).unwrap_err();
        assert!(err.contains("no repos are configured"));
    }

    #[test]
    fn resolve_repo_slug_ambiguous_with_many_repos() {
        let p = project_with_repos(&["org/a", "org/b"]);
        let err = p.resolve_repo_slug(None).unwrap_err();
        assert!(err.contains("multiple repos are configured"));
        assert!(err.contains("org/a"));
        assert!(err.contains("org/b"));
    }
}
