//! Fase review: a guarda do semântico. Abre uma sessão read-only com contexto
//! cheio (TODOS os RFCs/ADRs + diff dos PRs + checklist das constraints
//! `enforce: review`), grava o report linha a linha e decide `is_clean`.
//!
//! Garantia detectiva (obrigatória): `is_clean` só passa com ZERO findings E
//! todas as constraints `enforce: review` marcadas `ok` — patch 1, lado semântico.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor, Gh, Git, Session};
use jaum_core::{Enforce, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

/// Severidade de um achado. Só `Blocker`/`Major` reprovam o review; `Minor`/`Nit`
/// são informativos (não seguram o review em SUJO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    fn blocks(self) -> bool {
        matches!(self, Severity::Blocker | Severity::Major)
    }
    fn tag(self) -> &'static str {
        match self {
            Severity::Blocker => "BLOCKER",
            Severity::Major => "MAJOR",
            Severity::Minor => "MINOR",
            Severity::Nit => "NIT",
        }
    }
}

/// Default conservador: se o agente omitir, trata como `major` (reprova) para não
/// esconder problema por esquecimento.
fn default_severity() -> Severity {
    Severity::Major
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
    #[serde(default = "default_severity")]
    pub severity: Severity,
}

impl Finding {
    /// Este finding reprova o review?
    pub fn is_blocking(&self) -> bool {
        self.severity.blocks()
    }

    /// Linha "[SEV] arquivo:linha - mensagem [(ref)]".
    pub fn render(&self) -> String {
        let loc = match self.line {
            Some(l) => format!("{}:{}", self.file, l),
            None => self.file.clone(),
        };
        let tag = self.severity.tag();
        match &self.reference {
            Some(r) => format!("[{tag}] {loc} - {} (viola {r})", self.message),
            None => format!("[{tag}] {loc} - {}", self.message),
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
    /// Veredito de cada critério de aceite da task (mesmo shape das constraints).
    #[serde(default)]
    pub criteria: Vec<ConstraintResult>,
}

impl ReviewReport {
    /// Limpo se NÃO há finding bloqueante (blocker/major), todas as constraints
    /// `enforce: review` estão `ok` E todos os critérios de aceite foram atendidos
    /// (`ok`). Findings `minor`/`nit` aparecem mas não reprovam (senão um nitpick
    /// segura o review em SUJO pra sempre).
    pub fn is_clean(&self) -> bool {
        !self.findings.iter().any(|f| f.is_blocking())
            && self
                .constraints
                .iter()
                .all(|c| c.verdict == ConstraintVerdict::Ok)
            && self
                .criteria
                .iter()
                .all(|c| c.verdict == ConstraintVerdict::Ok)
    }

    /// Nº de itens que ainda reprovam/pendem: constraints + critérios não-`ok`.
    pub fn unmet_count(&self) -> usize {
        self.constraints
            .iter()
            .chain(self.criteria.iter())
            .filter(|c| c.verdict != ConstraintVerdict::Ok)
            .count()
    }

    /// Nº de findings bloqueantes (blocker/major).
    pub fn blocking_count(&self) -> usize {
        self.findings.iter().filter(|f| f.is_blocking()).count()
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
        render_checklist(&mut b, &self.constraints);

        b.push_str("\n## Critérios de aceite\n");
        render_checklist(&mut b, &self.criteria);
        b
    }
}

/// Flags read-only: nenhuma escrita. Whitelist de ferramentas de leitura é a
/// garantia mais forte de que o review não muta nada.
pub fn read_only_flags() -> ExecFlags {
    ExecFlags::new()
        .with_disallowed(["Edit", "Write", "NotebookEdit", "Bash"])
        .with_model(crate::AGENT_MODEL)
}

/// Orquestrador da fase review.
pub struct Review<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    gh: &'a Gh,
    executor: &'a E,
    docs_dir: PathBuf,
    /// Mapeamento explícito slug "owner/name" -> caminho local do repo.
    repos: HashMap<String, PathBuf>,
    /// Boas práticas do projeto (conventions.md).
    conventions: String,
}

impl<'a, E: Executor> Review<'a, E> {
    pub fn new(
        store: &'a Store,
        git: &'a Git,
        gh: &'a Gh,
        executor: &'a E,
        docs_dir: impl Into<PathBuf>,
        repos: HashMap<String, PathBuf>,
        conventions: impl Into<String>,
    ) -> Self {
        Self {
            store,
            git,
            gh,
            executor,
            docs_dir: docs_dir.into(),
            repos,
            conventions: conventions.into(),
        }
    }

    fn repo_path(&self, repo: &str) -> Option<PathBuf> {
        self.repos.get(repo).cloned()
    }

    /// cwd da sessão de review: o repo linkado da task (primeiro), senão o
    /// primeiro repo do projeto, senão o docs_dir. Assim o agente abre no código,
    /// não na home do jaum.
    pub fn review_cwd(&self, id: &str) -> PathBuf {
        if let Ok(task) = self.store.get(id) {
            for link in &task.prs {
                if let Some(p) = self.repos.get(&link.repo) {
                    return p.clone();
                }
            }
        }
        self.repos
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| self.docs_dir.clone())
    }

    /// Flags read-only + cwd no repo + acesso de leitura a todos os repos.
    fn review_flags(&self, id: &str) -> ExecFlags {
        let mut flags = read_only_flags();
        flags.cwd = Some(self.review_cwd(id));
        if !self.repos.is_empty() {
            flags.extra.push("--add-dir".to_string());
            flags
                .extra
                .extend(self.repos.values().map(|p| p.to_string_lossy().into_owned()));
        }
        flags
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

    /// Checklist obrigatório dos critérios de aceite da task (do corpo), cada um
    /// inicialmente `pending`. `is_clean` não passa enquanto algum não for `ok`.
    pub fn acceptance_checklist(&self, id: &str) -> Result<Vec<ConstraintResult>> {
        let task = self.store.get(id)?;
        Ok(task
            .acceptance_criteria()
            .into_iter()
            .map(|text| ConstraintResult {
                text,
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

        // 0) objetivo da task (o corpo, onde ficam objetivo e critérios de aceite)
        let body = task.body.trim();
        if !body.is_empty() {
            c.push_str("## O que a task pede\n\n");
            c.push_str(body);
            c.push_str("\n\n");
        }

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

        // 2) diff dos PRs — prefere o PR do GitHub (via gh); cai no diff local
        //    quando ainda não há PR aberto.
        c.push_str("## Diff dos PRs\n\n");
        for link in &task.prs {
            let repo_dir = self.repo_path(&link.repo);
            // resolve o número do PR (o salvo, ou descoberto pelo branch via gh,
            // rodando no diretório do repo).
            let pr = if link.pr != 0 {
                link.pr
            } else {
                repo_dir
                    .as_ref()
                    .and_then(|d| self.gh.pr_number(d, &link.branch).ok())
                    .unwrap_or(0)
            };

            match (pr, &repo_dir) {
                (pr, Some(dir)) if pr != 0 => {
                    if let Ok((title, body)) = self.gh.pr_view(dir, pr)
                        && !title.is_empty()
                    {
                        c.push_str(&format!("### {} PR #{pr}: {title}\n\n{body}\n\n", link.repo));
                    }
                    let diff = self.gh.pr_diff(dir, pr).unwrap_or_else(|e| {
                        format!("(não foi possível obter o diff do PR #{pr}: {e})")
                    });
                    c.push_str(&format!(
                        "#### diff do PR #{pr} ({} @ {})\n```diff\n{}\n```\n\n",
                        link.repo,
                        link.branch,
                        diff.trim()
                    ));
                }
                _ => {
                    // sem PR ainda: diff local do branch (fallback).
                    let diff = match &repo_dir {
                        Some(repo_path) => {
                            self.git.diff(repo_path, &link.branch).unwrap_or_else(|e| {
                                format!("(não foi possível obter diff de {}: {e})", link.repo)
                            })
                        }
                        None => format!("(repo {} não mapeado no projeto)", link.repo),
                    };
                    c.push_str(&format!(
                        "### {} @ {} (sem PR ainda - diff local)\n```diff\n{}\n```\n\n",
                        link.repo,
                        link.branch,
                        diff.trim()
                    ));
                }
            }
        }

        // 2.5) onde está o código, para ler além do diff
        c.push_str("## Working tree dos repos (leitura liberada)\n\n");
        if self.repos.is_empty() {
            c.push_str("(nenhum repo mapeado no projeto)\n\n");
        } else {
            let mut entries: Vec<_> = self.repos.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (slug, path) in entries {
                c.push_str(&format!("- {slug}: `{}`\n", path.display()));
            }
            c.push_str(
                "\nVocê PODE ler esses diretórios com Read/Grep/Glob (acesso liberado) para contexto \
além do diff: confirmar se arquivos/fixtures citados existem, ver código não alterado e conferir \
assinaturas. O working tree pode estar no branch base; o DIFF acima é a fonte de verdade do que mudou \
no PR. NÃO há Bash aqui (review é read-only) — não execute testes nem comandos, apenas leia.\n\n",
            );
        }

        // 3) convenções do projeto (sempre checadas)
        let conv = self.conventions.trim();
        if !conv.is_empty() {
            c.push_str("## Convenções do projeto a respeitar\n\n");
            c.push_str(conv);
            c.push_str("\n\n");
        }

        // 4) checklist obrigatório das constraints semânticas
        c.push_str("## Constraints a validar (enforce: review) — OBRIGATÓRIO\n\n");
        let checklist = self.check_semantic_constraints(id)?;
        if checklist.is_empty() {
            c.push_str("(nenhuma)\n");
        } else {
            for item in &checklist {
                c.push_str(&format!("- [ ] {}\n", item.text));
            }
        }

        // 5) checklist obrigatório dos critérios de aceite (do corpo da task)
        c.push_str(
            "\n## Critérios de aceite a validar — OBRIGATÓRIO\n\n\
Confirme, olhando o DIFF, se o que mudou realmente ATENDE cada critério abaixo. \
Marque `ok` só se o diff cumpre o critério; `reprovado` se não cumpre; `pending` se \
não dá para saber pelo diff.\n\n",
        );
        let criteria = self.acceptance_checklist(id)?;
        if criteria.is_empty() {
            c.push_str("(nenhum)\n");
        } else {
            for item in &criteria {
                c.push_str(&format!("- [ ] {}\n", item.text));
            }
        }
        Ok(c)
    }

    /// Abre a sessão de review read-only com o contexto cheio. Retorna a sessão e
    /// o UUID do claude (`--session-id`), para retomar depois.
    pub fn start(&self, id: &str) -> Result<(Session, String)> {
        let context = self.build_context(id)?;
        let claude_session_id = uuid::Uuid::new_v4().to_string();
        let flags = self.review_flags(id).with_session_id(&claude_session_id);
        let session = self.executor.spawn_interactive(&context, &flags)?;
        Ok((session, claude_session_id))
    }

    /// Retoma uma sessão de review read-only com `--resume`, sem reenviar o
    /// contexto (o claude recarrega a conversa). `cwd` deve ser o mesmo de origem.
    pub fn resume(&self, id: &str, uuid: &str, cwd: &Path) -> Result<Session> {
        let mut flags = self.review_flags(id);
        flags.cwd = Some(cwd.to_path_buf());
        flags = flags.with_resume(uuid);
        self.executor.spawn_interactive("", &flags)
    }

    /// Captura estruturada do review: roda `claude -p` read-only com o contexto
    /// cheio + schema, transforma a saída em `findings` + veredictos e GRAVA o
    /// `.review.md`. É o jaum quem garante a estrutura: a lista de constraints é a
    /// canônica (`enforce: review`); o claude só fornece o veredicto de cada uma.
    /// `on_line` recebe os logs ao vivo (resumo dos eventos do stream).
    pub fn capture_logged(
        &self,
        id: &str,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ReviewReport> {
        let prompt = format!("{}\n\n{REVIEW_INSTRUCTION}", self.build_context(id)?);

        let mut extra = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--json-schema".to_string(),
            review_schema().to_string(),
        ];
        if !self.repos.is_empty() {
            extra.push("--add-dir".to_string());
            extra.extend(self.repos.values().map(|p| p.to_string_lossy().into_owned()));
        }
        let mut flags = read_only_flags();
        flags.cwd = Some(self.review_cwd(id));
        flags.extra = extra;

        // reusa o resumo de eventos do ingest para os logs ao vivo.
        let mut summarize = |raw: &str| {
            for s in crate::ingest::summarize_event(raw) {
                on_line(&s);
            }
        };
        let out = self
            .executor
            .spawn_oneshot_streaming(&prompt, &flags, &mut summarize)?;

        let (findings, prop_constraints, prop_criteria) = parse_review_stream(&out)?;

        // as listas de constraints e critérios são canônicas (da task); o claude só
        // preenche o veredicto. Item não mencionado fica `pending`.
        let constraints = merge_verdicts(self.check_semantic_constraints(id)?, &prop_constraints);
        let criteria = merge_verdicts(self.acceptance_checklist(id)?, &prop_criteria);

        let report = ReviewReport {
            task: id.to_string(),
            findings,
            constraints,
            criteria,
        };
        self.write_report(&report)?;
        on_line(&format!(
            "review gravado: {} finding(s), {}",
            report.findings.len(),
            if report.is_clean() { "LIMPO" } else { "SUJO" }
        ));
        Ok(report)
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
        session.write_line(&handoff_message(&report))
    }
}

/// Mensagem de handoff: os findings + constraints reprovadas do review, para
/// injetar numa sessão de play corrigir. (Função pura, reusada pela TUI.)
pub fn handoff_message(report: &ReviewReport) -> String {
    let mut msg = String::from("Review apontou pendências, corrija:\n");
    for f in &report.findings {
        msg.push_str(&format!("- {}\n", f.render()));
    }
    for c in &report.constraints {
        if c.verdict == ConstraintVerdict::Reprovado {
            msg.push_str(&format!("- constraint reprovada: {} — {}\n", c.text, c.note));
        }
    }
    for c in &report.criteria {
        if c.verdict != ConstraintVerdict::Ok {
            msg.push_str(&format!("- critério não atendido: {} — {}\n", c.text, c.note));
        }
    }
    msg
}

/// Casa a lista canônica (da task) com os veredictos propostos pelo claude,
/// batendo pelo texto. Item não mencionado permanece como veio (`pending`).
fn merge_verdicts(
    canonical: Vec<ConstraintResult>,
    proposed: &[ConstraintResult],
) -> Vec<ConstraintResult> {
    canonical
        .into_iter()
        .map(|mut c| {
            if let Some(p) = proposed.iter().find(|p| p.text == c.text) {
                c.verdict = p.verdict;
                c.note = p.note.clone();
            }
            c
        })
        .collect()
}

/// Renderiza um checklist de veredictos (constraints ou critérios) no corpo.
fn render_checklist(b: &mut String, items: &[ConstraintResult]) {
    if items.is_empty() {
        b.push_str("- (nenhum)\n");
        return;
    }
    for c in items {
        let tag = match c.verdict {
            ConstraintVerdict::Ok => "OK",
            ConstraintVerdict::Reprovado => "REPROVADO",
            ConstraintVerdict::Pending => "PENDENTE",
        };
        if c.note.is_empty() {
            b.push_str(&format!("- [{tag}] {}\n", c.text));
        } else {
            b.push_str(&format!("- [{tag}] {} - {}\n", c.text, c.note));
        }
    }
}

/// Instrução anexada ao contexto para a captura estruturada do review.
const REVIEW_INSTRUCTION: &str = "Revise o diff acima contra os RFCs/ADRs, as convenções e o \
checklist de constraints. Seja EXAUSTIVO nesta única passada: aponte TODOS os problemas que \
encontrar de uma vez (não vá aos poucos), de bugs e violações a melhorias. Retorne SÓ pela saída \
estruturada: `findings` (cada um com `file`, `line` quando souber, `message`, `severity` e \
`reference` ao RFC/ADR violado quando aplicável), `constraints` (UMA entrada por item do checklist de \
constraints) e `criteria` (UMA entrada por item do checklist de critérios de aceite), cada uma \
com `verdict` `ok`/`reprovado`/`pending` e uma `note` curta. \
Classifique cada finding com `severity`: `blocker` (quebra/incorreto, tem que corrigir), `major` \
(importante), `minor` (melhoria) ou `nit` (cosmético). Só `blocker` e `major` reprovam o review; \
`minor`/`nit` são informativos. Repita o texto de cada constraint e de cada critério EXATAMENTE como \
está no checklist. Para os critérios de aceite, decida olhando o DIFF: `ok` se o que mudou cumpre o \
critério, `reprovado` se não cumpre, `pending` se o diff não permite decidir. \
Escreva `message` e `note` de forma CONCISA e direta: uma frase objetiva, sem travessões e sem \
floreio. Não altere nada; só leia. Seja rigoroso: só marque `ok` se for realmente cumprido.";

/// Schema da saída estruturada do review.
pub fn review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["findings", "constraints", "criteria"],
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["file", "message", "severity"],
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": ["integer", "null"] },
                        "message": { "type": "string" },
                        "severity": { "type": "string", "enum": ["blocker", "major", "minor", "nit"] },
                        "reference": { "type": ["string", "null"] }
                    }
                }
            },
            "constraints": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "verdict"],
                    "properties": {
                        "text": { "type": "string" },
                        "verdict": { "type": "string", "enum": ["ok", "reprovado", "pending"] },
                        "note": { "type": "string" }
                    }
                }
            },
            "criteria": {
                "type": "array",
                "description": "Um veredito por critério de aceite do checklist. O diff atende o critério?",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "verdict"],
                    "properties": {
                        "text": { "type": "string" },
                        "verdict": { "type": "string", "enum": ["ok", "reprovado", "pending"] },
                        "note": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// Extrai `findings` + `constraints` + `criteria` do último evento `result`.
#[allow(clippy::type_complexity)]
fn parse_review_stream(
    out: &str,
) -> Result<(Vec<Finding>, Vec<ConstraintResult>, Vec<ConstraintResult>)> {
    let mut last: Option<Value> = None;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v.get("type").and_then(Value::as_str) == Some("result")
        {
            last = Some(v);
        }
    }
    let v = last.context("stream-json sem evento `result` final")?;
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "claude reportou erro: {}",
            v.get("result").and_then(Value::as_str).unwrap_or("desconhecido")
        );
    }
    let so = v
        .get("structured_output")
        .context("saída sem `structured_output` (o --json-schema foi aplicado?)")?;
    let findings = match so.get("findings") {
        Some(f) => serde_json::from_value(f.clone()).context("desserializando findings")?,
        None => Vec::new(),
    };
    let constraints = match so.get("constraints") {
        Some(c) => serde_json::from_value(c.clone()).context("desserializando constraints")?,
        None => Vec::new(),
    };
    let criteria = match so.get("criteria") {
        Some(c) => serde_json::from_value(c.clone()).context("desserializando criteria")?,
        None => Vec::new(),
    };
    Ok((findings, constraints, criteria))
}

/// Lê os RFCs/ADRs (`RFC-*.md`, `ADR-*.md`) de um diretório, ordenados por nome.
fn collect_docs(docs_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if !docs_dir.exists() {
        return Ok(out);
    }
    for entry in
        fs::read_dir(docs_dir).with_context(|| format!("lendo docs em {}", docs_dir.display()))?
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
