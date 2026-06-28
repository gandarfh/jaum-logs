//! Fase play: prepara worktrees, monta o prompt apertado, instala os guards
//! (disallowedTools + PreToolUse hook das constraints `enforce: hook`) e a
//! reinjeção das constraints a cada turno (UserPromptSubmit hook — patch 2).
//!
//! Garantias mecânicas (hard):
//! - não-merge: `--disallowedTools` + o PreToolUse hook bloqueia `git merge` /
//!   `gh pr merge` SEMPRE (defesa dupla, independente das constraints).
//! - constraints `enforce: hook`: bloqueio preventivo via PreToolUse hook.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor, Git, Session};
use jaum_core::{Enforce, Status, Store, Task};
use serde_json::{Value, json};

/// Ferramentas de merge sempre bloqueadas via `--disallowedTools`. A garantia
/// real do não-merge é a dupla camada com o PreToolUse hook.
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

/// Prompt apertado da sessão: objetivo + contexto (RFCs/ADRs) + constraints +
/// regras de escopo. Inclui as `enforce: review` no corpo, para eu e o agente
/// as vermos.
pub fn build_prompt(task: &Task) -> String {
    let mut p = String::new();
    p.push_str(&format!("# {} ({:?})\n\n", task.id, task.task_type));

    if !task.body.trim().is_empty() {
        p.push_str(task.body.trim());
        p.push_str("\n\n");
    }

    if !task.rfcs.is_empty() || !task.adrs.is_empty() {
        p.push_str("## Contexto\n");
        if !task.rfcs.is_empty() {
            p.push_str(&format!("- RFCs: {}\n", task.rfcs.join(", ")));
        }
        if !task.adrs.is_empty() {
            p.push_str(&format!("- ADRs: {}\n", task.adrs.join(", ")));
        }
        p.push('\n');
    }

    p.push_str("## Constraints (NÃO viole)\n");
    p.push_str(&reinjection_text(task));
    p.push_str("\n\n");

    p.push_str("## Regras desta sessão\n");
    p.push_str("- Trabalhe só nesta task. Escopo extra: me avise para registrar como deferred (não amplie aqui).\n");
    p.push_str(
        "- Abra PR ao final. NUNCA faça merge — o merge é comando meu, fora desta ferramenta.\n",
    );
    p
}

/// Bloco de constraints reinjetado a cada turno (via UserPromptSubmit hook) e
/// também embutido no prompt/append-system-prompt. Separa as mecânicas (já
/// bloqueadas) das semânticas (responsabilidade do agente — foco do patch 2).
pub fn reinjection_text(task: &Task) -> String {
    let hooks = task.constraints_by(Enforce::Hook);
    let reviews = task.constraints_by(Enforce::Review);

    let mut t = String::new();
    if !hooks.is_empty() {
        t.push_str("Bloqueadas mecanicamente (o hook impede a ação):\n");
        for c in &hooks {
            t.push_str(&format!("- {}\n", c.text));
        }
    }
    if !reviews.is_empty() {
        t.push_str("Sua responsabilidade (sem bloqueio automático — respeite e serão checadas no review):\n");
        for c in &reviews {
            t.push_str(&format!("- {}\n", c.text));
        }
    }
    t.push_str("Lembrete fixo: NÃO faça merge; abra PR apenas.");
    t
}

/// Flags base do play: bloqueio de merge + constraints no system prompt.
/// `start` ainda adiciona o hook (`--settings`) e a `cwd` (worktree).
pub fn guard_flags(task: &Task) -> ExecFlags {
    ExecFlags::new()
        .with_disallowed(merge_disallowed())
        .with_append_system_prompt(reinjection_text(task))
}

/// Script bash do PreToolUse hook: bloqueia merge SEMPRE e cada constraint
/// `enforce: hook` pelo seu `hook_pattern()`. Robusto a falhas (não usa `set -e`,
/// para nunca encerrar antes de checar — fail-safe é bloquear, não liberar).
pub fn pretool_hook_script(task: &Task) -> String {
    let mut s = String::from(
        r#"#!/usr/bin/env bash
input=$(cat)
target=$(printf '%s' "$input" | jq -r '.tool_input.command // .tool_input.file_path // .tool_input.path // .tool_input.notebook_path // empty')
deny() {
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$1" | jq -Rs .)"
  exit 0
}
# não-merge (hard, sempre)
if printf '%s' "$target" | grep -qiE 'git[[:space:]]+merge|gh[[:space:]]+pr[[:space:]]+merge'; then
  deny "merge bloqueado pela ferramenta (PR-only; merge é comando seu)"
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

/// JSON de settings que registra os dois hooks. `pretool` aponta para o script;
/// `reinject` é impresso a cada UserPromptSubmit (reinjeção — patch 2).
pub fn settings_json(pretool_path: &Path, reinject_path: &Path) -> Value {
    json!({
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

/// Artefatos gerados para uma sessão de play.
pub struct Artifacts {
    pub dir: PathBuf,
    pub pretool_path: PathBuf,
    pub reinject_path: PathBuf,
    pub settings_path: PathBuf,
}

/// Orquestrador da fase play. Genérico no executor para manter a ferramenta
/// agnóstica (e testável com um executor de mentira).
pub struct Play<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    executor: &'a E,
    work_dir: PathBuf,
    /// Mapeamento explícito slug "owner/name" -> caminho local do repo.
    repos: HashMap<String, PathBuf>,
}

/// Sessão de play viva: a sessão do executor + as worktrees criadas.
pub struct PlaySession {
    pub id: String,
    pub session: Session,
    pub worktrees: Vec<(String, PathBuf)>,
    pub artifacts: Artifacts,
}

impl<'a, E: Executor> Play<'a, E> {
    pub fn new(
        store: &'a Store,
        git: &'a Git,
        executor: &'a E,
        work_dir: impl Into<PathBuf>,
        repos: HashMap<String, PathBuf>,
    ) -> Self {
        Self {
            store,
            git,
            executor,
            work_dir: work_dir.into(),
            repos,
        }
    }

    /// Resolve o slug "owner/name" para o caminho local mapeado no projeto.
    fn repo_path(&self, repo: &str) -> Result<PathBuf> {
        self.repos
            .get(repo)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("repo `{repo}` não está mapeado no projeto"))
    }

    /// Gera e grava os scripts dos hooks + o settings.json em `work_dir/<id>/`.
    pub fn install_hooks(&self, task: &Task) -> Result<Artifacts> {
        let dir = self.work_dir.join(&task.id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("criando dir de sessão {}", dir.display()))?;

        let pretool_path = dir.join("pretool.sh");
        let reinject_path = dir.join("reinject.txt");
        let settings_path = dir.join("settings.json");

        fs::write(&pretool_path, pretool_hook_script(task))?;
        make_executable(&pretool_path)?;
        fs::write(&reinject_path, reinjection_text(task))?;
        let settings = settings_json(&pretool_path, &reinject_path);
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

        Ok(Artifacts {
            dir,
            pretool_path,
            reinject_path,
            settings_path,
        })
    }

    /// Inicia o play: worktree por repo linkado, instala hooks, abre a sessão
    /// interativa com os guards e marca a task como `wip`. NUNCA mergeia.
    pub fn start(&self, id: &str) -> Result<PlaySession> {
        let task = self.store.get(id)?;
        if task.is_spike() {
            bail!("{id} é spike: gera documento (RFC/ADR), não tem play");
        }
        if task.prs.is_empty() {
            bail!("{id} não tem repo/branch linkado em `prs`");
        }

        let mut worktrees = Vec::new();
        for link in &task.prs {
            let repo_path = self.repo_path(&link.repo)?;
            self.git.branch_create(&repo_path, &link.branch)?;
            let wt = self.git.worktree_add(&repo_path, &link.branch)?;
            worktrees.push((link.repo.clone(), wt));
        }

        let artifacts = self.install_hooks(&task)?;

        let flags = guard_flags(&task)
            .with_hook(artifacts.settings_path.clone())
            .with_cwd(worktrees[0].1.clone());

        let prompt = build_prompt(&task);
        let session = self.executor.spawn_interactive(&prompt, &flags)?;

        self.store.set_status(id, Status::Wip)?;

        Ok(PlaySession {
            id: id.to_string(),
            session,
            worktrees,
            artifacts,
        })
    }

    /// Reinjeção manual (reforço): empurra o bloco de constraints no input da
    /// sessão agora. A reinjeção PRIMÁRIA é automática via UserPromptSubmit hook.
    pub fn reinject(&self, ps: &mut PlaySession) -> Result<()> {
        let task = self.store.get(&ps.id)?;
        ps.session.write_line(&reinjection_text(&task))
    }

    /// Encerra a sessão e remove as worktrees criadas.
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

/// Aspas simples seguras para bash (escapa `'`).
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
