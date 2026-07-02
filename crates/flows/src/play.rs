//! Play phase: prepares worktrees, builds the tight prompt, installs the guards
//! (disallowedTools + PreToolUse hook for `enforce: hook` constraints) and the
//! per-turn constraint reinjection (UserPromptSubmit hook).
//!
//! Mechanical (hard) guarantees:
//! - no-merge: `--disallowedTools` + the PreToolUse hook ALWAYS block `git merge` /
//!   `gh pr merge` (double defense, independent of the constraints).
//! - `enforce: hook` constraints: preventive block via the PreToolUse hook.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor, Git, Session};
use jaum_core::{Enforce, Status, Store, Task};
use serde_json::{Value, json};

/// Merge tools always blocked via `--disallowedTools`. The real no-merge
/// guarantee is the double layer with the PreToolUse hook.
fn merge_disallowed() -> Vec<String> {
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

/// Block reinjected every turn (via UserPromptSubmit hook) and embedded in the
/// system prompt. Combines: project conventions + task constraints (mechanical,
/// already blocked, vs semantic, the agent's responsibility).
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
        t.push_str("Mechanically blocked (the hook prevents the action):\n");
        for c in &hooks {
            t.push_str(&format!("- {}\n", c.text));
        }
    }
    if !reviews.is_empty() {
        t.push_str("Your responsibility (no automatic block — respect them; they are checked in review):\n");
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

/// Base play flags: merge block + constraints/conventions in the system prompt.
/// `start` also adds the hook (`--settings`) and the `cwd` (worktree).
pub fn guard_flags(task: &Task, conventions: &str) -> ExecFlags {
    ExecFlags::new()
        .with_disallowed(merge_disallowed())
        .with_append_system_prompt(reinjection_text(task, conventions))
        .with_model(crate::AGENT_MODEL)
}

/// Bash script for the PreToolUse hook: ALWAYS blocks merge and each
/// `enforce: hook` constraint via its `hook_pattern()`. Failure-robust (avoids
/// `set -e` so it never exits before checking — fail-safe is to block, not allow).
pub fn pretool_hook_script(task: &Task) -> String {
    let mut s = String::from(
        r#"#!/usr/bin/env bash
input=$(cat)
target=$(printf '%s' "$input" | jq -r '.tool_input.command // .tool_input.file_path // .tool_input.path // .tool_input.notebook_path // empty')
deny() {
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$1" | jq -Rs .)"
  exit 0
}
# no-merge (hard, always)
if printf '%s' "$target" | grep -qiE 'git[[:space:]]+merge|gh[[:space:]]+pr[[:space:]]+merge'; then
  deny "merge blocked by the tool (PR-only; merge is your command)"
fi
"#,
    );
    for c in task.constraints_by(Enforce::Hook) {
        let pat = sq(&c.hook_pattern());
        let reason = sq(&format!("constraint (enforce: hook): {}", c.text));
        s.push_str(&format!(
            "if printf '%s' \"$target\" | grep -qiE {pat}; then deny {reason}; fi\n"
        ));
    }
    s.push_str("exit 0\n");
    s
}

/// Settings JSON that registers both hooks. `pretool` points to the script;
/// `reinject` is printed on every UserPromptSubmit.
pub fn settings_json(pretool_path: &Path, reinject_path: &Path) -> Value {
    json!({
        // disable the "Co-Authored-By: Claude / Generated with Claude Code" trailer
        // on commits — no AI attribution in the repository.
        "includeCoAuthoredBy": false,
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash|Edit|Write|MultiEdit|NotebookEdit|Update",
                "hooks": [{ "type": "command", "command": pretool_path.to_string_lossy() }]
            }],
            "UserPromptSubmit": [{
                "hooks": [{ "type": "command", "command": format!("cat {}", sq(&reinject_path.to_string_lossy())) }]
            }]
        }
    })
}

/// Artifacts generated for a play session.
pub struct Artifacts {
    pub dir: PathBuf,
    pub pretool_path: PathBuf,
    pub reinject_path: PathBuf,
    pub settings_path: PathBuf,
}

/// Play phase orchestrator. Generic over the executor to keep the tool
/// agnostic (and testable with a fake executor).
pub struct Play<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    executor: &'a E,
    work_dir: PathBuf,
    /// Explicit slug "owner/name" -> local repo path mapping.
    repos: HashMap<String, PathBuf>,
    /// Project best practices (conventions.md), injected into every session.
    conventions: String,
}

/// A live play session: the executor's session + the created worktrees.
pub struct PlaySession {
    pub id: String,
    pub session: Session,
    pub worktrees: Vec<(String, PathBuf)>,
    pub artifacts: Artifacts,
    /// Claude session UUID (`--session-id`), used to resume later.
    pub claude_session_id: String,
}

impl<'a, E: Executor> Play<'a, E> {
    pub fn new(
        store: &'a Store,
        git: &'a Git,
        executor: &'a E,
        work_dir: impl Into<PathBuf>,
        repos: HashMap<String, PathBuf>,
        conventions: impl Into<String>,
    ) -> Self {
        Self {
            store,
            git,
            executor,
            work_dir: work_dir.into(),
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

    /// Generates and writes the hook scripts + settings.json in `work_dir/<id>/`.
    pub fn install_hooks(&self, task: &Task) -> Result<Artifacts> {
        let dir = self.work_dir.join(&task.id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;

        let pretool_path = dir.join("pretool.sh");
        let reinject_path = dir.join("reinject.txt");
        let settings_path = dir.join("settings.json");

        fs::write(&pretool_path, pretool_hook_script(task))?;
        make_executable(&pretool_path)?;
        fs::write(&reinject_path, reinjection_text(task, &self.conventions))?;
        let settings = settings_json(&pretool_path, &reinject_path);
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

        Ok(Artifacts {
            dir,
            pretool_path,
            reinject_path,
            settings_path,
        })
    }

    /// Starts play: a worktree per linked repo, installs hooks, opens the
    /// interactive session with the guards, and marks the task `wip`. NEVER merges.
    pub fn start(&self, id: &str) -> Result<PlaySession> {
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

        let artifacts = self.install_hooks(&task)?;

        let claude_session_id = uuid::Uuid::new_v4().to_string();
        let flags = guard_flags(&task, &self.conventions)
            .with_hook(artifacts.settings_path.clone())
            .with_cwd(worktrees[0].1.clone())
            .with_session_id(&claude_session_id);

        let prompt = build_prompt(&task, &self.conventions);
        let session = self.executor.spawn_interactive(&prompt, &flags)?;

        self.store.set_status(id, Status::Wip)?;

        Ok(PlaySession {
            id: id.to_string(),
            session,
            worktrees,
            artifacts,
            claude_session_id,
        })
    }

    /// Resumes an existing play session: relaunches claude with `--resume` in
    /// the SAME worktree (cwd), without resending the initial prompt or
    /// recreating the worktree, and without touching the status. Reinstalls the
    /// hooks (idempotent).
    pub fn resume(&self, id: &str, uuid: &str, cwd: &Path) -> Result<Session> {
        let task = self.store.get(id)?;
        let artifacts = self.install_hooks(&task)?;
        let flags = guard_flags(&task, &self.conventions)
            .with_hook(artifacts.settings_path)
            .with_cwd(cwd.to_path_buf())
            .with_resume(uuid);
        self.executor.spawn_interactive("", &flags)
    }

    /// Manual reinjection (reinforcement): pushes the constraint block into the
    /// session input now. The PRIMARY reinjection is automatic via the
    /// UserPromptSubmit hook.
    pub fn reinject(&self, ps: &mut PlaySession) -> Result<()> {
        let task = self.store.get(&ps.id)?;
        ps.session
            .write_line(&reinjection_text(&task, &self.conventions))
    }

    /// Ends the session and removes the created worktrees.
    pub fn stop(&self, ps: &mut PlaySession) -> Result<()> {
        let _ = ps.session.kill();
        for (repo, _wt) in &ps.worktrees {
            let repo_path = self.repo_path(repo)?;
            let task = self.store.get(&ps.id)?;
            if let Some(link) = task.prs.iter().find(|p| &p.repo == repo) {
                self.git.worktree_remove(&repo_path, &link.branch)?;
            }
        }
        Ok(())
    }
}

/// Bash-safe single quoting (escapes `'`).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}
