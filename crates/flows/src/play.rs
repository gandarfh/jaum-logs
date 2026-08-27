//! Play phase: prepares worktrees, builds the tight prompt, installs the guards
//! (disallowedTools + PreToolUse hook for `enforce: hook` constraints) and the
//! per-turn constraint reinjection (UserPromptSubmit hook), and spawns the
//! interactive `claude` session over a PTY via [`Executor`].
//!
//! Mechanical (hard) guarantees:
//! - no-merge: `--disallowedTools` + the PreToolUse hook ALWAYS block `git merge` /
//!   `gh pr merge` (double defense, independent of the task constraints).
//! - `enforce: hook` constraints: preventive deny via the PreToolUse hook,
//!   applied before asking for any approval.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor, Git, Session};
use jaum_core::{Enforce, Status, Store, Task};
use serde_json::{Value, json};

/// Merge tools always disallowed. The real no-merge guarantee is the double
/// layer with the PreToolUse hook's guard patterns.
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

/// A regex the PreToolUse hook applies to the tool target (command or file
/// path) to preemptively deny an `enforce: hook` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookGuard {
    pub pattern: String,
    pub reason: String,
}

/// Everything a session needs to keep the session guarded: the reinjected
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

/// Bash-safe single quoting (escapes `'`).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn make_executable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Bash script for the PreToolUse hook: ALWAYS blocks merge and each guard
/// pattern. Failure-robust (avoids `set -e` so it never exits before
/// checking — fail-safe is to block, not allow).
pub fn pretool_hook_script(guards: &[HookGuard]) -> String {
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
    for g in guards {
        let pat = sq(&g.pattern);
        let reason = sq(&format!("constraint (enforce: hook): {}", g.reason));
        s.push_str(&format!(
            "if printf '%s' \"$target\" | grep -qiE {pat}; then deny {reason}; fi\n"
        ));
    }
    s.push_str("exit 0\n");
    s
}

/// Settings JSON that registers both hooks. `pretool` points to the script;
/// `reinject` is printed on every UserPromptSubmit.
pub fn settings_json(pretool_path: &std::path::Path, reinject_path: &std::path::Path) -> Value {
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

/// Play phase orchestrator: worktrees + prompt + guards + the interactive
/// session, generic over the executor to keep the tool agnostic (and
/// testable with a fake executor).
pub struct Play<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    executor: &'a E,
    /// Explicit slug "owner/name" -> local repo path mapping.
    repos: HashMap<String, PathBuf>,
    /// Project best practices (conventions.md), injected into every session.
    conventions: String,
    /// Base directory where per-session hook artifacts are written.
    work_dir: PathBuf,
}

/// The initial turn of a play session, ready to be spawned.
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

/// A live play session: the executor's session + the created worktrees.
pub struct PlaySession {
    pub id: String,
    pub session: Session,
    pub worktrees: Vec<(String, PathBuf)>,
    pub cwd: PathBuf,
    pub artifacts: Artifacts,
    /// Claude session UUID (`--session-id` or `--resume`), used to resume later.
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
            repos,
            conventions: conventions.into(),
            work_dir: work_dir.into(),
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
    /// guards, and marks the task `wip`. NEVER merges. Doesn't spawn anything
    /// yet: [`Play::spawn`] takes the resulting [`PlayLaunch`] and opens the
    /// interactive session.
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

    /// Generates and writes the hook scripts + settings.json in
    /// `work_dir/<task_id>/`. Idempotent: safe to call again on resume.
    fn install_hooks(&self, task_id: &str, guard: &GuardSpec) -> Result<Artifacts> {
        let dir = self.work_dir.join(task_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;

        let pretool_path = dir.join("pretool.sh");
        let reinject_path = dir.join("reinject.txt");
        let settings_path = dir.join("settings.json");

        std::fs::write(&pretool_path, pretool_hook_script(&guard.guard_patterns))?;
        make_executable(&pretool_path)?;
        std::fs::write(&reinject_path, &guard.system_prompt_append)?;
        let settings = settings_json(&pretool_path, &reinject_path);
        std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

        Ok(Artifacts {
            dir,
            pretool_path,
            reinject_path,
            settings_path,
        })
    }

    fn base_flags(guard: &GuardSpec, settings_path: PathBuf, cwd: PathBuf) -> ExecFlags {
        ExecFlags::new()
            .with_disallowed(guard.disallowed_tools.clone())
            .with_append_system_prompt(guard.system_prompt_append.clone())
            .with_model(guard.model.clone())
            .with_hook(settings_path)
            .with_cwd(cwd)
    }

    /// Spawns the process for an ALREADY-launched session (worktrees created,
    /// status set by [`Play::launch`]). On `Err` the CALLER must roll those
    /// back (e.g. via [`Play::cleanup`]).
    pub fn spawn(&self, launch: &PlayLaunch, resume: Option<&str>) -> Result<PlaySession> {
        let artifacts = self.install_hooks(&launch.id, &launch.guards)?;
        let mut flags = Self::base_flags(
            &launch.guards,
            artifacts.settings_path.clone(),
            launch.cwd.clone(),
        );
        let claude_session_id = match resume {
            Some(uuid) => {
                flags = flags.with_resume(uuid);
                uuid.to_string()
            }
            None => {
                flags = flags.with_session_id(&launch.session_id);
                launch.session_id.clone()
            }
        };
        let prompt = if resume.is_some() {
            ""
        } else {
            launch.prompt.as_str()
        };
        let session = self.executor.spawn_interactive(prompt, &flags)?;
        Ok(PlaySession {
            id: launch.id.clone(),
            session,
            worktrees: launch.worktrees.clone(),
            cwd: launch.cwd.clone(),
            artifacts,
            claude_session_id,
        })
    }

    /// Boot-time reattach ONLY: the worktree from the previous run is still on
    /// disk at `cwd` (verified by the caller before this is invoked). Never
    /// touches git or the task status; just recomputes the guards, reinstalls
    /// the hooks (idempotent) and relaunches claude with `--resume`.
    pub fn resume(
        &self,
        id: &str,
        claude_session_id: &str,
        cwd: &std::path::Path,
    ) -> Result<Session> {
        let guard = self.resume_spec(id)?;
        let artifacts = self.install_hooks(id, &guard)?;
        let flags = Self::base_flags(&guard, artifacts.settings_path, cwd.to_path_buf())
            .with_resume(claude_session_id);
        self.executor.spawn_interactive("", &flags)
    }

    /// Ends the session and removes the created worktrees.
    pub fn stop(&self, ps: &mut PlaySession) -> Result<()> {
        let _ = ps.session.kill();
        self.cleanup(&ps.id, &ps.worktrees)
    }
}
