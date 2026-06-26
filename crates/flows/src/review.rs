//! Fase review: a guarda do semântico. Abre uma sessão read-only com contexto
//! cheio (TODOS os RFCs/ADRs + diff dos PRs + checklist das constraints
//! `enforce: review`), grava o report linha a linha e decide `is_clean`.
//!
//! Garantia detectiva (obrigatória): `is_clean` só passa com ZERO findings E
//! todas as constraints `enforce: review` marcadas `ok` — patch 1, lado semântico.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jaum_adapters::{ExecFlags, Executor, Git, Session};
use jaum_core::{Enforce, Store};
use serde::{Deserialize, Serialize};

/// Veredito de uma constraint semântica no review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintVerdict {
    /// Ainda não avaliada — `is_clean` falha enquanto houver pendência.
    Pending,
    Ok,
    Reprovado,
}

/// Resultado da checagem de uma constraint `enforce: review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintResult {
    pub text: String,
    pub verdict: ConstraintVerdict,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Um achado do review, ancorado em arquivo:linha e (idealmente) num RFC/ADR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
    /// RFC/ADR violado, se aplicável.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

impl Finding {
    /// Linha "arquivo:linha — mensagem [(ref)]".
    pub fn render(&self) -> String {
        let loc = match self.line {
            Some(l) => format!("{}:{}", self.file, l),
            None => self.file.clone(),
        };
        match &self.reference {
            Some(r) => format!("{loc} — {} (viola {r})", self.message),
            None => format!("{loc} — {}", self.message),
        }
    }
}

/// Report persistido em `.backlog/TASK-NNN.review.md` (frontmatter estruturado +
/// corpo legível). É o estado de verdade do review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub task: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub constraints: Vec<ConstraintResult>,
}

impl ReviewReport {
    /// Limpo SÓ se zero findings E todas as constraints `enforce: review` `ok`.
    /// Pendente ou reprovado reprova.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
            && self
                .constraints
                .iter()
                .all(|c| c.verdict == ConstraintVerdict::Ok)
    }

    /// Renderiza o corpo markdown legível (linha a linha).
    fn render_body(&self) -> String {
        let mut b = format!("# Review {}\n\n", self.task);
        b.push_str(&format!(
            "**Resultado:** {}\n\n",
            if self.is_clean() { "LIMPO" } else { "SUJO" }
        ));

        b.push_str("## Findings\n");
        if self.findings.is_empty() {
            b.push_str("- (nenhum)\n");
        } else {
            for f in &self.findings {
                b.push_str(&format!("- {}\n", f.render()));
            }
        }

        b.push_str("\n## Constraints (enforce: review)\n");
        if self.constraints.is_empty() {
            b.push_str("- (nenhuma)\n");
        } else {
            for c in &self.constraints {
                let tag = match c.verdict {
                    ConstraintVerdict::Ok => "OK",
                    ConstraintVerdict::Reprovado => "REPROVADO",
                    ConstraintVerdict::Pending => "PENDENTE",
                };
                if c.note.is_empty() {
                    b.push_str(&format!("- [{tag}] {}\n", c.text));
                } else {
                    b.push_str(&format!("- [{tag}] {} — {}\n", c.text, c.note));
                }
            }
        }
        b
    }
}

/// Flags read-only: nenhuma escrita. Whitelist de ferramentas de leitura é a
/// garantia mais forte de que o review não muta nada.
pub fn read_only_flags() -> ExecFlags {
    ExecFlags::new().with_disallowed([
        "Edit",
        "Write",
        "MultiEdit",
        "NotebookEdit",
        "Update",
        "Bash",
    ])
}

/// Orquestrador da fase review.
pub struct Review<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    executor: &'a E,
    docs_dir: PathBuf,
    repos_root: PathBuf,
}

impl<'a, E: Executor> Review<'a, E> {
    pub fn new(
        store: &'a Store,
        git: &'a Git,
        executor: &'a E,
        docs_dir: impl Into<PathBuf>,
        repos_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            git,
            executor,
            docs_dir: docs_dir.into(),
            repos_root: repos_root.into(),
        }
    }

    fn repo_path(&self, repo: &str) -> PathBuf {
        let name = repo.rsplit('/').next().unwrap_or(repo);
        self.repos_root.join(name)
    }

    /// Checklist obrigatório: cada constraint `enforce: review` vira um item a
    /// validar, inicialmente `pending`. `is_clean` não passa enquanto pendente.
    pub fn check_semantic_constraints(&self, id: &str) -> Result<Vec<ConstraintResult>> {
        let task = self.store.get(id)?;
        Ok(task
            .constraints_by(Enforce::Review)
            .into_iter()
            .map(|c| ConstraintResult {
                text: c.text.clone(),
                verdict: ConstraintVerdict::Pending,
                note: String::new(),
            })
            .collect())
    }

    /// Contexto cheio do review: TODOS os RFCs/ADRs do projeto + diff dos PRs da
    /// task + as constraints `enforce: review` como checklist obrigatório.
    pub fn build_context(&self, id: &str) -> Result<String> {
        let task = self.store.get(id)?;
        let mut c = String::new();
        c.push_str(&format!("# Review read-only de {}\n\n", task.id));
        c.push_str("Você é revisor. NÃO altere nada. Aponte findings como `arquivo:linha — o que viola (RFC/ADR)`.\n\n");

        // 1) todos os docs do projeto
        c.push_str("## RFCs/ADRs do projeto\n\n");
        let docs = collect_docs(&self.docs_dir)?;
        if docs.is_empty() {
            c.push_str("(nenhum doc encontrado em ");
            c.push_str(&self.docs_dir.to_string_lossy());
            c.push_str(")\n\n");
        } else {
            for (name, body) in &docs {
                c.push_str(&format!("### {name}\n{body}\n\n"));
            }
        }

        // 2) diff dos PRs
        c.push_str("## Diff dos PRs\n\n");
        for link in &task.prs {
            let repo_path = self.repo_path(&link.repo);
            let diff = self.git.diff(&repo_path, &link.branch).unwrap_or_else(|e| {
                format!("(não foi possível obter diff de {}: {e})", link.repo)
            });
            c.push_str(&format!("### {} @ {}\n```diff\n{}\n```\n\n", link.repo, link.branch, diff.trim()));
        }

        // 3) checklist obrigatório das constraints semânticas
        c.push_str("## Constraints a validar (enforce: review) — OBRIGATÓRIO\n\n");
        let checklist = self.check_semantic_constraints(id)?;
        if checklist.is_empty() {
            c.push_str("(nenhuma)\n");
        } else {
            for item in &checklist {
                c.push_str(&format!("- [ ] {}\n", item.text));
            }
        }
        Ok(c)
    }

    /// Abre a sessão de review read-only com o contexto cheio.
    pub fn start(&self, id: &str) -> Result<Session> {
        let context = self.build_context(id)?;
        self.executor.spawn_interactive(&context, &read_only_flags())
    }

    /// Grava o report em `.backlog/TASK-NNN.review.md`.
    pub fn write_report(&self, report: &ReviewReport) -> Result<()> {
        let path = self.store.review_path(&report.task);
        self.store.write_doc(&path, report, &report.render_body())
    }

    /// Carrega o report persistido.
    pub fn load_report(&self, id: &str) -> Result<ReviewReport> {
        let path = self.store.review_path(id);
        let (report, _body) = self.store.read_doc::<ReviewReport>(&path)?;
        Ok(report)
    }

    /// `true` só se o report persistido estiver limpo (zero findings E todas as
    /// `enforce: review` ok).
    pub fn is_clean(&self, id: &str) -> Result<bool> {
        Ok(self.load_report(id)?.is_clean())
    }

    /// Injeta os findings na sessão de play da mesma task (handoff do sujo).
    pub fn handoff(&self, id: &str, session: &mut Session) -> Result<()> {
        let report = self.load_report(id)?;
        let mut msg = String::from("Review apontou pendências, corrija:\n");
        for f in &report.findings {
            msg.push_str(&format!("- {}\n", f.render()));
        }
        for c in &report.constraints {
            if c.verdict == ConstraintVerdict::Reprovado {
                msg.push_str(&format!("- constraint reprovada: {} — {}\n", c.text, c.note));
            }
        }
        session.write_line(&msg)
    }
}

/// Lê os RFCs/ADRs (`RFC-*.md`, `ADR-*.md`) de um diretório, ordenados por nome.
fn collect_docs(docs_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if !docs_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(docs_dir)
        .with_context(|| format!("lendo docs em {}", docs_dir.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if (name.starts_with("RFC-") || name.starts_with("ADR-")) && name.ends_with(".md") {
            let body = fs::read_to_string(&path).unwrap_or_default();
            out.push((name.to_string(), body));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
