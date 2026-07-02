//! Fase setup: a sessão de configuração inicial do projeto. Diferente do play
//! (PR-only) e do review (read-only), aqui o claude PODE escrever — mas só na
//! área externa do jaum (`~/jaum/<proj>`: backlog, conventions.md, setup.md). Os
//! repos entram apenas como leitura (`--add-dir`) para o agente entender o
//! projeto. É um chat interativo: o usuário e o claude iteram até o setup ficar
//! bom (vincular tasks a repos, escrever convenções, mapear repo↔área).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use jaum_adapters::{ExecFlags, Executor, Session};
use jaum_core::Store;

/// Ferramentas de merge sempre bloqueadas (defesa: o setup não mergeia/pusha).
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

/// Orquestrador da sessão de setup.
pub struct Setup<'a, E: Executor> {
    store: &'a Store,
    executor: &'a E,
    /// Pasta externa do projeto (`~/jaum/<proj>`): cwd da sessão (escrita aqui).
    home: PathBuf,
    /// Repos do projeto (slug -> caminho local), montados read-only via add-dir.
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

    /// Flags da sessão: escrita liberada (no cwd = jaum home), repos read-only via
    /// `--add-dir`, e merge/push bloqueados por garantia.
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

    /// Monta o prompt do setup: o que é obrigatório, os dados do projeto (tasks,
    /// repos, estado das convenções) e como gravar no backlog.
    pub fn build_prompt(&self) -> Result<String> {
        let tasks = self.store.list(None)?;
        let mut p = String::new();

        p.push_str("# Setup do projeto (modo configuração do jaum)\n\n");
        p.push_str(
            "Você está configurando ESTE projeto no jaum (nossa ferramenta interna de organização). \
Pode editar arquivos NESTE diretório (a área externa do jaum: `backlog/`, `conventions.md`, `setup.md`). \
Os repositórios estão montados só para leitura (contexto). Não faça merge/push.\n\n",
        );

        p.push_str("## Contrato\n");
        p.push_str(
            "- O jaum é interno: o que vai para o repositório (branch, commit, PR, código, comentário) \
descreve o TRABALHO, não a nossa organização interna — o id da task (TASK-xxx) e o jaum não aparecem lá.\n",
        );
        p.push_str(
            "- Frontmatter de `backlog/TASK-*.md`: `id`, `type`, `status`, `rfcs`, `adrs` (do jaum, ficam só \
aqui) e `prs` = lista de `{ repo: \"<slug>\", pr: 0, branch: \"<branch>\" }`. Toda task de implementação tem \
ao menos um `prs`: o repo correto (slugs abaixo) e um branch que descreve o trabalho em kebab-case \
(ex.: `feat/markdown-deck-parser`).\n\n",
        );

        p.push_str("## Seu trabalho (iterativo, conversando comigo)\n");
        p.push_str("1. Vincular cada task ao repo + branch certos (campo `prs`).\n");
        p.push_str("2. Preencher `conventions.md` se ainda estiver no template.\n");
        p.push_str("3. Escrever `setup.md` com o mapeamento repo↔área/RFC.\n");
        p.push_str(
            "Comece resumindo o que falta e seu plano; proponha e confirme comigo antes de mudanças grandes.\n\n",
        );

        // repos disponíveis
        p.push_str("## Repos do projeto (slugs válidos para `prs`)\n");
        if self.repos.is_empty() {
            p.push_str("(nenhum repo mapeado — avise o usuário para rodar `jaum init` no repo)\n\n");
        } else {
            let mut slugs: Vec<_> = self.repos.iter().collect();
            slugs.sort_by(|a, b| a.0.cmp(b.0));
            for (slug, path) in slugs {
                p.push_str(&format!("- {slug}  ({})\n", path.display()));
            }
            p.push('\n');
        }

        // tasks do backlog
        p.push_str("## Tasks no backlog\n");
        if tasks.is_empty() {
            p.push_str("(backlog vazio — rode o ingest antes do setup)\n\n");
        } else {
            for t in &tasks {
                let refs: Vec<String> = t.rfcs.iter().chain(t.adrs.iter()).cloned().collect();
                let linked = if t.prs.is_empty() {
                    "SEM repo".to_string()
                } else {
                    t.prs
                        .iter()
                        .map(|pr| {
                            if branch_leaks_id(&pr.branch) {
                                format!("{}@{} (branch vaza o id — renomeie)", pr.repo, pr.branch)
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

        // estado das convenções
        p.push_str("## conventions.md\n");
        if is_template(&self.conventions) {
            p.push_str("(ainda no template/vazio — preencha com as boas práticas do projeto)\n");
        } else {
            p.push_str("(já preenchido — só ajuste se precisar)\n");
        }

        Ok(p)
    }

    /// Abre a sessão interativa de setup. Retorna a sessão e o UUID do claude
    /// (`--session-id`), para retomar depois.
    pub fn start(&self) -> Result<(Session, String)> {
        let prompt = self.build_prompt()?;
        let claude_session_id = uuid::Uuid::new_v4().to_string();
        let flags = self.flags().with_session_id(&claude_session_id);
        let session = self.executor.spawn_interactive(&prompt, &flags)?;
        Ok((session, claude_session_id))
    }

    /// Retoma a sessão de setup com `--resume`, sem reenviar o prompt (o claude
    /// recarrega a conversa). O cwd do setup é sempre o jaum home.
    pub fn resume(&self, uuid: &str) -> Result<Session> {
        let flags = self.flags().with_resume(uuid);
        self.executor.spawn_interactive("", &flags)
    }
}

/// `conventions.md` ainda está no template (vazio ou só o scaffold, sem nenhuma
/// convenção real escrita)? Um bullet real é um `- ` seguido de texto.
pub fn is_template(conventions: &str) -> bool {
    let c = conventions.trim();
    c.is_empty()
        || !c.lines().any(|l| {
            let l = l.trim();
            l.starts_with("- ") && l[2..].trim().len() > 1
        })
}

/// O branch vaza um id interno do jaum (padrão `task-<n>`)? Invariante imposta
/// pelo jaum: branches descrevem o trabalho, não a contabilidade interna.
pub fn branch_leaks_id(branch: &str) -> bool {
    let b = branch.to_lowercase();
    b.match_indices("task-")
        .any(|(i, _)| b[i + 5..].chars().next().is_some_and(|c| c.is_ascii_digit()))
}
