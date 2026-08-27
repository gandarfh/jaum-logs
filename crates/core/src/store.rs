use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gray_matter::Matter;
use gray_matter::engine::YAML;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::JaumError;
use crate::model::{Constraint, Enforce, Repo, Status, Task, TaskType};

/// Owner of the `.backlog/` directory: the single source of truth for the backlog.
/// Every task read/write goes through here.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store pointing at `.backlog/` relative to the current directory.
    pub fn open() -> Self {
        Self::new(".backlog")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.md"))
    }

    // --- reads -------------------------------------------------------------

    /// Reads and parses a task from an arbitrary path.
    pub fn parse(&self, path: &Path) -> Result<Task> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("reading task at {}", path.display()))?;

        let matter = Matter::<YAML>::new();
        let parsed = matter
            .parse::<Task>(&content)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("parsing frontmatter of {}", path.display()))?;

        let mut task = parsed.data.ok_or_else(|| JaumError::MalformedFrontmatter {
            path: path.display().to_string(),
        })?;
        task.body = parsed.content.trim().to_string();
        task.path = Some(path.to_path_buf());
        Ok(task)
    }

    /// Lists tasks, optionally filtering by status. Sorted by id.
    pub fn list(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("reading directory {}", self.root.display()))?
        {
            let path = entry?.path();
            if !is_task_file(&path) {
                continue;
            }
            let task = self.parse(&path)?;
            if status.is_none_or(|s| task.status == s) {
                out.push(task);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Fetches the task by id (convention: `TASK-NNN.md` file).
    pub fn get(&self, id: &str) -> Result<Task> {
        let path = self.task_path(id);
        if !path.exists() {
            bail!(JaumError::TaskNotFound(id.to_string()));
        }
        self.parse(&path)
    }

    // --- writes ------------------------------------------------------------

    /// Writes the task as markdown with YAML frontmatter. Uses `task.path` if
    /// present, otherwise `.backlog/{id}.md`.
    pub fn write(&self, task: &Task) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating directory {}", self.root.display()))?;

        let yaml = serde_yaml_ng::to_string(task).context("serializing frontmatter")?;
        // Normalize whether or not the crate prefixes `---`.
        let yaml = yaml.trim_start_matches("---\n").trim_end();
        let body = task.body.trim();
        let content = format!("---\n{yaml}\n---\n\n{body}\n");

        let path = task
            .path
            .clone()
            .unwrap_or_else(|| self.task_path(&task.id));
        fs::write(&path, content).with_context(|| format!("writing task at {}", path.display()))?;
        Ok(())
    }

    // --- generic markdown+frontmatter IO (review reports, docs) ------------

    /// Writes `meta` (YAML frontmatter) + `body` to an arbitrary path.
    pub fn write_doc<T: Serialize>(&self, path: &Path, meta: &T, body: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let yaml = serde_yaml_ng::to_string(meta).context("serializing frontmatter")?;
        let yaml = yaml.trim_start_matches("---\n").trim_end();
        let content = format!("---\n{yaml}\n---\n\n{}\n", body.trim());
        fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Reads `(frontmatter, body)` from a markdown file with YAML frontmatter.
    pub fn read_doc<T: DeserializeOwned>(&self, path: &Path) -> Result<(T, String)> {
        let content =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let matter = Matter::<YAML>::new();
        let parsed = matter
            .parse::<T>(&content)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("parsing frontmatter of {}", path.display()))?;
        let data = parsed.data.ok_or_else(|| JaumError::MalformedFrontmatter {
            path: path.display().to_string(),
        })?;
        Ok((data, parsed.content.trim().to_string()))
    }

    /// Creates an `impl`/`backlog` task stub from RFC references.
    pub fn create_stub(&self, rfc_refs: &[String]) -> Result<Task> {
        let id = self.next_id()?;
        let task = Task {
            id: id.clone(),
            task_type: TaskType::Impl,
            status: Status::Backlog,
            rfcs: rfc_refs.to_vec(),
            adrs: Vec::new(),
            prs: Vec::new(),
            deferred: Vec::new(),
            constraints: Vec::new(),
            locks: Vec::new(),
            body: "## Objective\n\n## Acceptance criteria\n".to_string(),
            path: Some(self.task_path(&id)),
        };
        self.write(&task)?;
        Ok(task)
    }

    /// Creates a rich backlog (used by ingest): type, RFC/ADR refs and body.
    pub fn create_backlog(
        &self,
        task_type: TaskType,
        rfcs: Vec<String>,
        adrs: Vec<String>,
        body: String,
    ) -> Result<Task> {
        let id = self.next_id()?;
        let task = Task {
            id: id.clone(),
            task_type,
            status: Status::Backlog,
            rfcs,
            adrs,
            prs: Vec::new(),
            deferred: Vec::new(),
            constraints: Vec::new(),
            locks: Vec::new(),
            body,
            path: Some(self.task_path(&id)),
        };
        self.write(&task)?;
        Ok(task)
    }

    pub fn set_status(&self, id: &str, status: Status) -> Result<Task> {
        let mut task = self.get(id)?;
        task.status = status;
        self.write(&task)?;
        Ok(task)
    }

    /// Links a repo+branch to the task (creates the `PrLink` with `pr: 0` if it
    /// doesn't exist, or updates the branch if the repo was already linked). Used in play setup.
    pub fn link_repo(&self, id: &str, repo: &str, branch: &str) -> Result<Task> {
        let mut task = self.get(id)?;
        if let Some(link) = task.prs.iter_mut().find(|p| p.repo == repo) {
            link.branch = branch.to_string();
        } else {
            task.prs.push(crate::model::PrLink {
                repo: repo.to_string(),
                pr: 0,
                branch: branch.to_string(),
            });
        }
        self.write(&task)?;
        Ok(task)
    }

    /// Records the number of an existing PR (read from `gh`) on the repo's link.
    pub fn set_pr(&self, id: &str, repo: &str, pr_num: u64) -> Result<Task> {
        let mut task = self.get(id)?;
        let link = task
            .prs
            .iter_mut()
            .find(|p| p.repo == repo)
            .ok_or_else(|| JaumError::PrLinkNotFound {
                id: id.to_string(),
                repo: repo.to_string(),
            })?;
        link.pr = pr_num;
        self.write(&task)?;
        Ok(task)
    }

    /// Records extra scope in `deferred` and materializes a new backlog from it —
    /// the guard against the "infinite project". Returns the created task.
    pub fn add_deferred(&self, id: &str, text: &str) -> Result<Task> {
        let mut origin = self.get(id)?;
        origin.deferred.push(text.to_string());
        self.write(&origin)?;

        let new_id = self.next_id()?;
        let spawned = Task {
            id: new_id.clone(),
            task_type: TaskType::Impl,
            status: Status::Backlog,
            rfcs: Vec::new(),
            adrs: Vec::new(),
            prs: Vec::new(),
            deferred: Vec::new(),
            constraints: Vec::new(),
            locks: Vec::new(),
            body: format!(
                "## Objective\n\n{text}\n\n_Derived from {id}._\n\n## Acceptance criteria\n"
            ),
            path: Some(self.task_path(&new_id)),
        };
        self.write(&spawned)?;
        Ok(spawned)
    }

    // --- derived queries --------------------------------------------------

    pub fn linked_repos(&self, id: &str) -> Result<Vec<Repo>> {
        Ok(self.get(id)?.linked_repos())
    }

    pub fn constraints(&self, id: &str, kind: Enforce) -> Result<Vec<Constraint>> {
        Ok(self
            .get(id)?
            .constraints
            .into_iter()
            .filter(|c| c.enforce == kind)
            .collect())
    }

    // --- internals ---------------------------------------------------------

    /// Next sequential id `TASK-NNN` (highest existing number + 1).
    fn next_id(&self) -> Result<String> {
        let mut max = 0u32;
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path();
                if let Some(n) = parse_task_number(&path) {
                    max = max.max(n);
                }
            }
        }
        Ok(format!("TASK-{:03}", max + 1))
    }
}

/// Task file: `TASK-*.md`, excluding the `*.review.md` reports.
fn is_task_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("TASK-") && name.ends_with(".md") && !name.ends_with(".review.md")
}

/// Extracts the number from `TASK-NNN...` in the file name (review files included).
fn parse_task_number(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("TASK-")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
