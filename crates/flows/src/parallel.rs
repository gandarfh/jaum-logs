//! Análise de paralelismo: usa o LLM para decidir quais tasks REALMENTE colidem
//! (mesmos arquivos/área no mesmo repo), não só "compartilham repo". É read-only;
//! grava um relatório INTERNO consumido pelo Board — nunca escreve no repositório.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor};
use jaum_core::{Status, Store, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ingest::summarize_event;

/// Um conflito entre duas tasks: elas se cruzam (mesmos arquivos/área) e não
/// devem rodar em paralelo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub a: String,
    pub b: String,
    pub repo: String,
    pub reason: String,
}

/// Relatório de paralelismo: os pares que colidem. Tasks sem aresta entre si
/// podem rodar em paralelo.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParallelReport {
    pub conflicts: Vec<Conflict>,
}

impl ParallelReport {
    /// O conflito entre duas tasks (em qualquer ordem), se houver.
    pub fn conflict_between(&self, x: &str, y: &str) -> Option<&Conflict> {
        self.conflicts
            .iter()
            .find(|c| (c.a == x && c.b == y) || (c.a == y && c.b == x))
    }
}

/// Orquestrador da análise. Genérico no executor (testável com fake).
pub struct Parallel<'a, E: Executor> {
    store: &'a Store,
    executor: &'a E,
    /// cwd da varredura (raiz do projeto ou docs); só precisa ser um dir válido.
    root: PathBuf,
    /// Repos liberados para leitura (`--add-dir`), onde a análise inspeciona o código.
    repos: HashMap<String, PathBuf>,
}

impl<'a, E: Executor> Parallel<'a, E> {
    pub fn new(
        store: &'a Store,
        executor: &'a E,
        root: impl Into<PathBuf>,
        repos: HashMap<String, PathBuf>,
    ) -> Self {
        Self {
            store,
            executor,
            root: root.into(),
            repos,
        }
    }

    /// Flags read-only + acesso de leitura aos repos + saída estruturada.
    fn flags(&self) -> ExecFlags {
        let mut extra = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--json-schema".to_string(),
            schema().to_string(),
        ];
        if !self.repos.is_empty() {
            extra.push("--add-dir".to_string());
            extra.extend(self.repos.values().map(|p| p.to_string_lossy().into_owned()));
        }
        ExecFlags {
            disallowed_tools: ["Edit", "Write", "NotebookEdit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            cwd: Some(self.root.clone()),
            extra,
            ..Default::default()
        }
    }

    /// Analisa as tasks abertas (não-merged) e devolve os pares que colidem.
    /// `on_line` recebe um resumo legível de cada evento (logs ao vivo).
    pub fn analyze_logged(&self, on_line: &mut dyn FnMut(&str)) -> Result<ParallelReport> {
        let open: Vec<Task> = self
            .store
            .list(None)?
            .into_iter()
            .filter(|t| t.status != Status::Merged)
            .collect();
        // menos de duas tasks: nada a comparar.
        if open.len() < 2 {
            return Ok(ParallelReport::default());
        }
        let prompt = build_prompt(&open);
        let flags = self.flags();
        let mut summarize = |raw: &str| {
            for s in summarize_event(raw) {
                on_line(&s);
            }
        };
        let out = self
            .executor
            .spawn_oneshot_streaming(&prompt, &flags, &mut summarize)?;
        parse_stream(&out)
    }
}

/// Extrai os conflitos da saída `stream-json`: o último evento `result` traz o
/// envelope com `structured_output`.
pub fn parse_stream(out: &str) -> Result<ParallelReport> {
    let mut last_result: Option<Value> = None;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v.get("type").and_then(Value::as_str) == Some("result")
        {
            last_result = Some(v);
        }
    }
    let v = last_result.context("stream-json sem evento `result` final")?;
    parse_envelope(&v)
}

/// Extrai os conflitos de um envelope `result` (json ou último evento do stream).
pub fn parse_structured(out: &str) -> Result<ParallelReport> {
    let v: Value = serde_json::from_str(out.trim())
        .context("parseando a saída JSON do claude (--output-format json)")?;
    parse_envelope(&v)
}

fn parse_envelope(v: &Value) -> Result<ParallelReport> {
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
    let conflicts = match so.get("conflicts") {
        Some(c) => serde_json::from_value(c.clone()).context("desserializando os conflitos")?,
        None => Vec::new(),
    };
    Ok(ParallelReport { conflicts })
}

/// Schema da saída estruturada.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["conflicts"],
        "properties": {
            "conflicts": {
                "type": "array",
                "description": "Apenas os PARES de tasks que realmente colidem (mesmos arquivos/área no mesmo repo). Quem não aparece aqui pode rodar em paralelo.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["a", "b", "repo", "reason"],
                    "properties": {
                        "a": { "type": "string", "description": "id da primeira task (ex.: TASK-002)" },
                        "b": { "type": "string", "description": "id da segunda task" },
                        "repo": { "type": "string", "description": "slug do repo onde colidem (ex.: owner/name)" },
                        "reason": { "type": "string", "description": "Por que colidem, curto e concreto (ex.: ambas editam src/render.rs)." }
                    }
                }
            }
        }
    })
}

/// Prompt da análise: lista as tasks abertas e pede os pares que colidem.
pub fn build_prompt(tasks: &[Task]) -> String {
    let mut p = String::new();
    p.push_str(
        "Você analisa quais tarefas podem rodar EM PARALELO sem conflito. Duas tasks \
conflitam quando, ao serem implementadas, mexeriam nos MESMOS arquivos ou na mesma \
área do código (no mesmo repositório). Tasks em repositórios diferentes NUNCA \
conflitam. Estar no mesmo repo NÃO é conflito por si só: só conflita se as áreas se \
cruzam de verdade.\n\n\
Inspecione os repositórios (somente leitura) para confirmar onde cada task mexeria. \
Seja preciso: na dúvida, considere que NÃO conflitam (paralelo é o padrão). Reporte \
apenas os pares que colidem.\n\n## Tasks abertas\n\n",
    );
    for t in tasks {
        let repos = t.linked_repos().join(", ");
        p.push_str(&format!("### {} (repos: {})\n", t.id, if repos.is_empty() { "nenhum".into() } else { repos }));
        let body = t.body.trim();
        if !body.is_empty() {
            p.push_str(body);
            p.push('\n');
        }
        p.push('\n');
    }
    p
}
