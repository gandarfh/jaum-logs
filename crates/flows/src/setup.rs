//! Setup phase: the project's initial configuration session. Unlike play
//! (PR-only) and review (read-only), here claude CAN write — but only in jaum's
//! external area (`~/jaum/<proj>`: backlog, conventions.md, setup.md). The repos
//! come in read-only (`--add-dir`) so the agent can understand the project. It's
//! an interactive chat: the user and claude iterate until the setup is good
//! (link tasks to repos, write conventions, map repo↔area).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use jaum_adapters::{ExecFlags, Executor, Session};
use jaum_core::Store;

/// Merge tools always blocked (defense: setup never merges/pushes).
fn merge_disallowed() -> Vec<String> {
    [
        "Bash(git merge)",
        "Bash(git merge:*)",
        "Bash(gh pr merge)",
        "Bash(gh pr merge:*)",
        "Bash(git push)",
        "Bash(git push:*)",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Setup session orchestrator.
pub struct Setup<'a, E: Executor> {
    store: &'a Store,
    executor: &'a E,
    /// Project's external folder (`~/jaum/<proj>`): the session cwd (writes here).
    home: PathBuf,
    /// Project repos (slug -> local path), mounted read-only via add-dir.
    repos: HashMap<String, PathBuf>,
    conventions: String,
}

impl<'a, E: Executor> Setup<'a, E> {
    pub fn new(
        store: &'a Store,
        executor: &'a E,
        home: impl Into<PathBuf>,
        repos: HashMap<String, PathBuf>,
        conventions: impl Into<String>,
    ) -> Self {
        Self {
            store,
            executor,
            home: home.into(),
            repos,
            conventions: conventions.into(),
        }
    }

    /// Session flags: writes allowed (in cwd = jaum home), repos read-only via
    /// `--add-dir`, and merge/push blocked as a safeguard.
    fn flags(&self) -> ExecFlags {
        let mut extra = Vec::new();
        if !self.repos.is_empty() {
            extra.push("--add-dir".to_string());
            extra.extend(self.repos.values().map(|p| p.to_string_lossy().into_owned()));
        }
        ExecFlags {
            disallowed_tools: merge_disallowed(),
            cwd: Some(self.home.clone()),
            model: Some(crate::AGENT_MODEL.to_string()),
            extra,
            ..Default::default()
        }
    }

    /// Builds the setup prompt: what's required, the project data (tasks,
    /// repos, conventions state) and how to write to the backlog.
    pub fn build_prompt(&self) -> Result<String> {
        let tasks = self.store.list(None)?;
        let mut p = String::new();

        p.push_str("# Project setup (jaum configuration mode)\n\n");
        p.push_str(
            "You are configuring THIS project in jaum (our internal organization tool). \
You may edit files in THIS directory (jaum's external area: `backlog/`, `conventions.md`, `setup.md`). \
The repositories are mounted read-only (context). Do not merge or push.\n\n",
        );

        p.push_str("## Contract\n");
        p.push_str(
            "- jaum is internal: what goes to the repository (branch, commit, PR, code, comment) \
describes the WORK, not our internal organization — the task id (TASK-xxx) and jaum never appear there.\n",
        );
        p.push_str(
            "- Frontmatter of `backlog/TASK-*.md`: `id`, `type`, `status`, `rfcs`, `adrs` (jaum's, stay \
here only) and `prs` = list of `{ repo: \"<slug>\", pr: 0, branch: \"<branch>\" }`. Every implementation \
task has at least one `prs`: the correct repo (slugs below) and a branch that describes the work in \
kebab-case (e.g. `feat/markdown-deck-parser`).\n\n",
        );

        p.push_str("## Your work (iterative, in conversation with me)\n");
        p.push_str("1. Link each task to the right repo + branch (`prs` field).\n");
        p.push_str("2. Fill in `conventions.md` if it's still the template.\n");
        p.push_str("3. Write `setup.md` with the repo↔area/RFC mapping.\n");
        p.push_str(
            "Start by summarizing what's missing and your plan; propose and confirm with me before big changes.\n\n",
        );

        // available repos
        p.push_str("## Project repos (valid slugs for `prs`)\n");
        if self.repos.is_empty() {
            p.push_str("(no repo mapped — tell the user to run `jaum init` in the repo)\n\n");
        } else {
            let mut slugs: Vec<_> = self.repos.iter().collect();
            slugs.sort_by(|a, b| a.0.cmp(b.0));
            for (slug, path) in slugs {
                p.push_str(&format!("- {slug}  ({})\n", path.display()));
            }
            p.push('\n');
        }

        // backlog tasks
        p.push_str("## Backlog tasks\n");
        if tasks.is_empty() {
            p.push_str("(backlog empty — run ingest before setup)\n\n");
        } else {
            for t in &tasks {
                let refs: Vec<String> = t.rfcs.iter().chain(t.adrs.iter()).cloned().collect();
                let linked = if t.prs.is_empty() {
                    "NO repo".to_string()
                } else {
                    t.prs
                        .iter()
                        .map(|pr| {
                            if branch_leaks_id(&pr.branch) {
                                format!("{}@{} (branch leaks the id — rename it)", pr.repo, pr.branch)
                            } else {
                                format!("{}@{}", pr.repo, pr.branch)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let obj = t.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                p.push_str(&format!(
                    "- {} [{}] refs={:?} -> {}\n  {}\n",
                    t.id,
                    format!("{:?}", t.task_type).to_lowercase(),
                    refs,
                    linked,
                    obj.trim()
                ));
            }
            p.push('\n');
        }

        // conventions state
        p.push_str("## conventions.md\n");
        if is_template(&self.conventions) {
            p.push_str("(still template/empty — fill it with the project's best practices)\n");
        } else {
            p.push_str("(already filled — adjust only if needed)\n");
        }

        Ok(p)
    }

    /// Opens the interactive setup session. Returns the session and claude's
    /// UUID (`--session-id`), to resume later.
    pub fn start(&self) -> Result<(Session, String)> {
        let prompt = self.build_prompt()?;
        let claude_session_id = uuid::Uuid::new_v4().to_string();
        let flags = self.flags().with_session_id(&claude_session_id);
        let session = self.executor.spawn_interactive(&prompt, &flags)?;
        Ok((session, claude_session_id))
    }

    /// Resumes the setup session with `--resume`, without resending the prompt
    /// (claude reloads the conversation). The setup cwd is always the jaum home.
    pub fn resume(&self, uuid: &str) -> Result<Session> {
        let flags = self.flags().with_resume(uuid);
        self.executor.spawn_interactive("", &flags)
    }
}

/// Is `conventions.md` still the template (empty or just the scaffold, with no
/// real convention written)? A real bullet is a `- ` followed by text.
pub fn is_template(conventions: &str) -> bool {
    let c = conventions.trim();
    c.is_empty()
        || !c.lines().any(|l| {
            let l = l.trim();
            l.starts_with("- ") && l[2..].trim().len() > 1
        })
}

/// Does the branch leak an internal jaum id (pattern `task-<n>`)? Invariant
/// enforced by jaum: branches describe the work, not internal bookkeeping.
pub fn branch_leaks_id(branch: &str) -> bool {
    let b = branch.to_lowercase();
    b.match_indices("task-")
        .any(|(i, _)| b[i + 5..].chars().next().is_some_and(|c| c.is_ascii_digit()))
}
