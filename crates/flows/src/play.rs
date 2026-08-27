//! Play phase: prepares worktrees, builds the tight prompt and describes the
//! mechanical guards for the session. Execution goes through the sidecar (the
//! daemon sends the prompt as a chat turn); this module stays pure so the
//! flow is testable without spawning anything.
//!
//! Mechanical (hard) guarantees:
//! - no-merge: the merge tools are disallowed AND the sidecar's tool guard
//!   always denies `git merge` / `gh pr merge` (double defense, independent
//!   of the task constraints).
//! - `enforce: hook` constraints: preventive deny via guard patterns applied
//!   by the sidecar before asking for any approval.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use jaum_adapters::Git;
use jaum_core::{Enforce, Status, Store, Task};

/// Merge tools always disallowed. The real no-merge guarantee is the double
/// layer with the sidecar's guard patterns.
pub fn merge_disallowed() -> Vec<String> {
    [
        "Bash(git merge)",
        "Bash(git merge:*)",
        "Bash(gh pr merge)",
        "Bash(gh pr merge:*)",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A regex the sidecar applies to the tool target (command or file path) to
/// preemptively deny an `enforce: hook` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookGuard {
    pub pattern: String,
    pub reason: String,
}

/// Everything a chat turn needs to keep the session guarded: the reinjected
/// system prompt, the blocked tools and the constraint patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardSpec {
    pub system_prompt_append: String,
    pub disallowed_tools: Vec<String>,
    pub guard_patterns: Vec<HookGuard>,
    pub model: String,
}

/// Builds the guard spec of a task: conventions + constraints + repo map in
/// the system prompt, merge disallowed, and one guard pattern per
/// `enforce: hook` constraint.
pub fn guard_spec(task: &Task, conventions: &str, repos: &HashMap<String, PathBuf>) -> GuardSpec {
    let mut append = reinjection_text(task, conventions);
    let repo_map = repo_map_text(task, repos);
    if !repo_map.is_empty() {
        append.push_str("\n\n");
        append.push_str(&repo_map);
    }
    GuardSpec {
        system_prompt_append: append,
        disallowed_tools: merge_disallowed(),
        guard_patterns: task
            .constraints_by(Enforce::Hook)
            .iter()
            .map(|c| HookGuard {
                pattern: c.hook_pattern(),
                reason: c.text.clone(),
            })
            .collect(),
        model: crate::AGENT_MODEL.to_string(),
    }
}

/// Compact map of the project's repositories for the system prompt, so the
/// agent knows which slug lives where; repos linked to the task carry their
/// branch. Sorted by slug for a stable prompt.
pub fn repo_map_text(task: &Task, repos: &HashMap<String, PathBuf>) -> String {
    if repos.is_empty() {
        return String::new();
    }
    let mut slugs: Vec<&String> = repos.keys().collect();
    slugs.sort();
    let mut t = String::from("Repository map (slug: local path):\n");
    for slug in slugs {
        let path = repos[slug].display();
        match task.prs.iter().find(|p| &p.repo == slug) {
            Some(link) => {
                t.push_str(&format!(
                    "- {slug}: {path} (this task, branch {})\n",
                    link.branch
                ));
            }
            None => t.push_str(&format!("- {slug}: {path}\n")),
        }
    }
    t
}

/// Tight session prompt: objective + context (RFCs/ADRs) + constraints + scope
/// rules. Includes `enforce: review` in the body so both the user and the agent
/// can see them.
pub fn build_prompt(task: &Task, conventions: &str) -> String {
    let mut p = String::new();
    p.push_str(&format!("# {} ({:?})\n\n", task.id, task.task_type));

    if !task.body.trim().is_empty() {
        p.push_str(task.body.trim());
        p.push_str("\n\n");
    }

    if !task.rfcs.is_empty() || !task.adrs.is_empty() {
        p.push_str("## Context\n");
        if !task.rfcs.is_empty() {
            p.push_str(&format!("- RFCs: {}\n", task.rfcs.join(", ")));
        }
        if !task.adrs.is_empty() {
            p.push_str(&format!("- ADRs: {}\n", task.adrs.join(", ")));
        }
        p.push('\n');
    }

    p.push_str("## Constraints and conventions (do NOT violate)\n");
    p.push_str(&reinjection_text(task, conventions));
    p.push_str("\n\n");

    p.push_str("## Session rules\n");
    p.push_str("- Work only on this task. Extra scope: tell me so I can log it as deferred (do not expand it here).\n");
    p.push_str("- Open a PR at the end. NEVER merge — merging is my command, outside this tool.\n");
    p
}

/// Constraint block embedded in the system prompt and reinjected every turn.
/// Combines: project conventions + task constraints (mechanical, already
/// blocked, vs semantic, the agent's responsibility).
pub fn reinjection_text(task: &Task, conventions: &str) -> String {
    let hooks = task.constraints_by(Enforce::Hook);
    let reviews = task.constraints_by(Enforce::Review);

    let mut t = String::new();
    let conv = conventions.trim();
    if !conv.is_empty() {
        t.push_str("Project conventions (always apply):\n");
        t.push_str(conv);
        t.push_str("\n\n");
    }
    if !hooks.is_empty() {
        t.push_str("Mechanically blocked (the tool guard prevents the action):\n");
        for c in &hooks {
            t.push_str(&format!("- {}\n", c.text));
        }
    }
    if !reviews.is_empty() {
        t.push_str("Your responsibility (no automatic block — nothing checks this for you now):\n");
        for c in &reviews {
            t.push_str(&format!("- {}\n", c.text));
        }
    }
    t.push_str(
        "Fixed reminder: do NOT merge; open a PR only. NEVER expose the internal jaum tool \
or the task id (TASK-xxx) in a branch, commit, PR, code, or comment; describe the work, \
not the internal bookkeeping.\n",
    );
    t.push_str(
        "Output to the repository (commits, PR title and body, code, comments): write in \
ENGLISH, direct and pragmatic. No em dashes (use commas, parentheses, or colons). \
No emojis and no AI attribution (no \"Generated with Claude Code\", \
\"Co-Authored-By: Claude\", or similar).",
    );
    t
}

/// Play phase orchestrator: worktrees + prompt + guards. The daemon owns the
/// actual chat over the sidecar.
pub struct Play<'a> {
    store: &'a Store,
    git: &'a Git,
    /// Explicit slug "owner/name" -> local repo path mapping.
    repos: HashMap<String, PathBuf>,
    /// Project best practices (conventions.md), injected into every session.
    conventions: String,
}

/// The initial turn of a play session, ready to be sent as a chat command.
pub struct PlayLaunch {
    pub id: String,
    /// Session uuid: keys the event log and is forced as the claude session
    /// id, so resume works from the same identifier.
    pub session_id: String,
    pub prompt: String,
    /// First worktree; the session's working directory.
    pub cwd: PathBuf,
    pub worktrees: Vec<(String, PathBuf)>,
    pub guards: GuardSpec,
}

impl<'a> Play<'a> {
    pub fn new(
        store: &'a Store,
        git: &'a Git,
        repos: HashMap<String, PathBuf>,
        conventions: impl Into<String>,
    ) -> Self {
        Self {
            store,
            git,
            repos,
            conventions: conventions.into(),
        }
    }

    /// Resolves the slug "owner/name" to the local path mapped in the project.
    fn repo_path(&self, repo: &str) -> Result<PathBuf> {
        self.repos
            .get(repo)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("repo `{repo}` is not mapped in the project"))
    }

    /// Starts play: a worktree per linked repo, the initial prompt and the
    /// guards, and marks the task `wip`. NEVER merges.
    pub fn launch(&self, id: &str) -> Result<PlayLaunch> {
        let task = self.store.get(id)?;
        if task.is_spike() {
            bail!("{id} is a spike: it produces a document (RFC/ADR), no play");
        }
        if task.prs.is_empty() {
            bail!("{id} has no repo/branch linked in `prs`");
        }

        let mut worktrees = Vec::new();
        for link in &task.prs {
            let repo_path = self.repo_path(&link.repo)?;
            self.git.branch_create(&repo_path, &link.branch)?;
            let wt = self.git.worktree_add(&repo_path, &link.branch)?;
            worktrees.push((link.repo.clone(), wt));
        }

        self.store.set_status(id, Status::Wip)?;

        Ok(PlayLaunch {
            id: id.to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            prompt: build_prompt(&task, &self.conventions),
            cwd: worktrees[0].1.clone(),
            worktrees,
            guards: guard_spec(&task, &self.conventions, &self.repos),
        })
    }

    /// Guards for a follow-up turn of an existing session (resume): the same
    /// constraint block, recomputed so edits to the task or conventions apply.
    pub fn resume_spec(&self, id: &str) -> Result<GuardSpec> {
        let task = self.store.get(id)?;
        Ok(guard_spec(&task, &self.conventions, &self.repos))
    }

    /// Removes the worktrees of a launched session. The branch stays in the
    /// repo (the worktree is just the working copy).
    pub fn cleanup(&self, id: &str, worktrees: &[(String, PathBuf)]) -> Result<()> {
        let task = self.store.get(id)?;
        for (repo, _wt) in worktrees {
            if let Some(link) = task.prs.iter().find(|p| &p.repo == repo) {
                let repo_path = self.repo_path(repo)?;
                self.git.worktree_remove(&repo_path, &link.branch)?;
            }
        }
        Ok(())
    }
}
