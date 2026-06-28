//! Ingest com LLM: o `claude -p` (headless, read-only) varre o projeto, acha os
//! documentos (RFC/ADR/PRD/spec/design — como quer que o projeto os chame e
//! organize) e devolve stubs em JSON validado por schema. O jaum (e só ele)
//! grava o `.backlog/`.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor};
use jaum_core::{Store, Task, TaskType};
use serde::Deserialize;
use serde_json::{Value, json};

/// Uma task proposta pelo agente (antes de virar backlog no disco).
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedTask {
    pub title: String,
    #[serde(rename = "type", default = "default_type")]
    pub task_type: String,
    #[serde(default)]
    pub rfcs: Vec<String>,
    #[serde(default)]
    pub adrs: Vec<String>,
    #[serde(default)]
    pub objetivo: String,
    #[serde(default)]
    pub criterio: Vec<String>,
}

fn default_type() -> String {
    "impl".to_string()
}

impl ProposedTask {
    fn task_type(&self) -> TaskType {
        match self.task_type.to_lowercase().as_str() {
            "spike" => TaskType::Spike,
            _ => TaskType::Impl,
        }
    }

    fn body(&self) -> String {
        let mut b = format!(
            "## Objetivo\n\n{}\n\n## Criterio de aceite\n",
            self.objetivo.trim()
        );
        if self.criterio.is_empty() {
            b.push_str("- [ ] \n");
        } else {
            for c in &self.criterio {
                b.push_str(&format!("- [ ] {}\n", c.trim()));
            }
        }
        b
    }

    /// Chave de dedup: refs normalizadas e ordenadas.
    fn ref_key(&self) -> Vec<String> {
        let mut refs: Vec<String> = self
            .rfcs
            .iter()
            .chain(self.adrs.iter())
            .map(|r| r.to_uppercase())
            .collect();
        refs.sort();
        refs
    }
}

/// Orquestrador do ingest.
pub struct Ingest<'a, E: Executor> {
    store: &'a Store,
    executor: &'a E,
    root: PathBuf,
    add_dirs: Vec<PathBuf>,
}

impl<'a, E: Executor> Ingest<'a, E> {
    pub fn new(
        store: &'a Store,
        executor: &'a E,
        root: impl Into<PathBuf>,
        add_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            store,
            executor,
            root: root.into(),
            add_dirs,
        }
    }

    /// Roda a varredura via `claude -p` e devolve as tasks propostas.
    pub fn scan(&self) -> Result<Vec<ProposedTask>> {
        let mut extra = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            schema().to_string(),
        ];
        if !self.add_dirs.is_empty() {
            extra.push("--add-dir".to_string());
            extra.extend(
                self.add_dirs
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned()),
            );
        }
        // read-only: o agente lê e reporta; o jaum é quem grava o backlog
        let flags = ExecFlags {
            disallowed_tools: ["Edit", "Write", "NotebookEdit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            cwd: Some(self.root.clone()),
            extra,
            ..Default::default()
        };
        let out = self.executor.spawn_oneshot(&build_prompt(), &flags)?;
        parse_structured(&out)
    }

    /// Varre e materializa os stubs no `.backlog/`, pulando duplicatas (mesmo
    /// conjunto de refs já presente). Devolve as tasks criadas.
    pub fn run(&self) -> Result<Vec<Task>> {
        let proposed = self.scan()?;
        create_stubs(self.store, &proposed)
    }
}

/// Cria os stubs no store, deduplicando por conjunto de refs já existente.
pub fn create_stubs(store: &Store, proposed: &[ProposedTask]) -> Result<Vec<Task>> {
    let existing = store.list(None)?;
    let mut seen: HashSet<Vec<String>> = existing
        .iter()
        .map(|t| {
            let mut refs: Vec<String> = t
                .rfcs
                .iter()
                .chain(t.adrs.iter())
                .map(|r| r.to_uppercase())
                .collect();
            refs.sort();
            refs
        })
        .filter(|r| !r.is_empty())
        .collect();

    let mut created = Vec::new();
    for p in proposed {
        let key = p.ref_key();
        if !key.is_empty() && seen.contains(&key) {
            continue; // já existe um backlog para essas refs
        }
        let task = store.create_backlog(
            p.task_type(),
            p.rfcs.iter().map(|r| r.to_uppercase()).collect(),
            p.adrs.iter().map(|r| r.to_uppercase()).collect(),
            p.body(),
        )?;
        if !key.is_empty() {
            seen.insert(key);
        }
        created.push(task);
    }
    Ok(created)
}

/// Extrai as tasks do envelope JSON do `claude --output-format json` (campo
/// `structured_output`, validado pelo `--json-schema`).
pub fn parse_structured(out: &str) -> Result<Vec<ProposedTask>> {
    let v: Value = serde_json::from_str(out.trim())
        .context("parseando a saída JSON do claude (--output-format json)")?;
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "claude reportou erro: {}",
            v.get("result")
                .and_then(Value::as_str)
                .unwrap_or("desconhecido")
        );
    }
    let so = v
        .get("structured_output")
        .context("saída sem `structured_output` (o --json-schema foi aplicado?)")?;
    let tasks = so
        .get("tasks")
        .context("`structured_output` sem campo `tasks`")?;
    serde_json::from_value(tasks.clone()).context("desserializando as tasks propostas")
}

/// Schema da saída estruturada exigida do agente.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tasks"],
        "properties": {
            "tasks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "type", "objetivo"],
                    "properties": {
                        "title": { "type": "string" },
                        "type": { "type": "string", "enum": ["impl", "spike"] },
                        "rfcs": { "type": "array", "items": { "type": "string" } },
                        "adrs": { "type": "array", "items": { "type": "string" } },
                        "objetivo": { "type": "string" },
                        "criterio": { "type": "array", "items": { "type": "string" } }
                    }
                }
            }
        }
    })
}

/// Prompt do agente de ingest.
pub fn build_prompt() -> String {
    r#"Você está inicializando o backlog de tarefas da ferramenta `jaum` varrendo ESTE projeto.

Objetivo: encontre TODOS os documentos de design, requisito ou decisão — RFCs, ADRs, PRDs, specs, design docs, propostas — não importa como este projeto os nomeie ou organize (qualquer pasta, qualquer convenção de nome, maiúsculo ou minúsculo, com ou sem frontmatter). Procure na raiz do projeto e em todos os repositórios disponíveis.

Para cada unidade coerente de trabalho de implementação implicada por esses documentos, gere um stub de task de backlog. Regras:
- "type": "impl" para trabalho de implementação; "spike" para trabalho exploratório que deve produzir um documento (RFC/ADR) em vez de um PR.
- "rfcs"/"adrs": os IDs dos documentos que motivam a task, usando a convenção do próprio projeto (ex.: RFC-0001, PRD-05, ADR-3). Normalize para maiúsculas.
- "objetivo": 1 a 3 frases descrevendo a meta.
- "criterio": alguns itens de critério de aceite.
- Seja conservador: uma task por unidade real de trabalho; NÃO exploda cada documento em dezenas de tasks. Prefira ~1 task por PRD/RFC, a menos que claramente contenha entregáveis independentes.
- ADRs são decisões/registros; só vire task se implicarem trabalho concreto pendente.

NÃO escreva nem modifique nenhum arquivo. Apenas leia. Retorne o resultado pela saída estruturada (structured output)."#
        .to_string()
}
