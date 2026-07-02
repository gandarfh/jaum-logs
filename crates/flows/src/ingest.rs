//! Ingest com LLM: o `claude -p` (headless, read-only) varre o projeto, acha os
//! documentos (RFC/ADR/PRD/spec/design — como quer que o projeto os chame e
//! organize) e devolve stubs em JSON validado por schema. O jaum (e só ele)
//! grava o `.backlog/`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor};
use jaum_core::{Store, Task, TaskType};
use serde::Deserialize;
use serde_json::{Value, json};

/// Resultado bruto de uma varredura: tasks propostas + documentos encontrados
/// (já classificados pelo agente, para o jaum espelhar organizado em `docs/`).
#[derive(Debug, Default)]
pub struct ProposedScan {
    pub tasks: Vec<ProposedTask>,
    pub docs: Vec<ProposedDoc>,
}

/// Um documento descoberto: caminho de origem + classificação que o agente deu
/// lendo o CONTEÚDO (não o nome do arquivo) e um nome canônico padronizado.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedDoc {
    /// Caminho absoluto do arquivo de origem (no repo ou no docs_dir).
    pub path: String,
    /// Categoria: rfc | adr | prd | design | spec | other.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Nome canônico proposto (ex.: `ADR-0001-protocolo-como-anotacao.md`).
    #[serde(default)]
    pub name: String,
}

fn default_kind() -> String {
    "other".to_string()
}

/// Pasta de destino para cada categoria de doc.
fn folder_for(kind: &str) -> &'static str {
    match kind.to_lowercase().as_str() {
        "rfc" | "rfcs" => "rfcs",
        "adr" | "adrs" => "adrs",
        "prd" | "prds" => "prd",
        "design" => "design",
        "spec" | "specs" => "specs",
        _ => "outros",
    }
}

/// Nome canônico do destino: usa o `name` proposto (só o basename, com `.md`),
/// caindo no nome original do arquivo quando o agente não propôs um.
fn dest_name(doc: &ProposedDoc, src: &Path) -> String {
    let raw = if doc.name.trim().is_empty() {
        src.file_name().and_then(|n| n.to_str()).unwrap_or("doc.md").to_string()
    } else {
        doc.name.trim().to_string()
    };
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("doc.md");
    if base.to_lowercase().ends_with(".md") {
        base.to_string()
    } else {
        format!("{base}.md")
    }
}

/// Remove diretórios vazios sob `root` (não remove o próprio `root`). Limpa
/// pastas que ficaram órfãs depois de mover docs para suas categorias.
fn prune_empty_dirs(root: &Path) {
    fn walk(dir: &Path, root: &Path) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root);
            }
        }
        if dir != root
            && fs::read_dir(dir)
                .map(|mut r| r.next().is_none())
                .unwrap_or(false)
        {
            let _ = fs::remove_dir(dir);
        }
    }
    walk(root, root);
}

/// Resultado final de um ingest/captura: stubs criados + docs copiados.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    pub created: Vec<Task>,
    pub docs_imported: usize,
}

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

    /// Monta o `--add-dir <repos>` comum às duas formas de varredura.
    fn add_dir_args(&self) -> Vec<String> {
        let mut extra = Vec::new();
        if !self.add_dirs.is_empty() {
            extra.push("--add-dir".to_string());
            extra.extend(
                self.add_dirs
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned()),
            );
        }
        extra
    }

    /// Flags read-only de varredura (o agente lê e reporta; o jaum é quem grava).
    fn scan_flags(&self, extra: Vec<String>) -> ExecFlags {
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

    /// Roda um prompt de varredura via `claude -p` (read-only, saída estruturada).
    fn run_scan(&self, prompt: &str) -> Result<ProposedScan> {
        let mut extra = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            schema().to_string(),
        ];
        extra.extend(self.add_dir_args());
        let flags = self.scan_flags(extra);
        let out = self.executor.spawn_oneshot(prompt, &flags)?;
        parse_structured(&out)
    }

    /// Varre os docs e devolve as tasks propostas.
    pub fn scan(&self) -> Result<ProposedScan> {
        self.run_scan(&build_prompt())
    }

    /// Como `run_scan`, mas em modo streaming: usa `stream-json` e repassa um
    /// resumo legível de cada evento para `on_line` enquanto o claude trabalha.
    fn run_scan_logged(
        &self,
        prompt: &str,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ProposedScan> {
        let mut extra = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--json-schema".to_string(),
            schema().to_string(),
        ];
        extra.extend(self.add_dir_args());
        let flags = self.scan_flags(extra);
        // o executor entrega linhas cruas (JSON); resumimos para texto amigável.
        let mut summarize = |raw: &str| {
            for s in summarize_event(raw) {
                on_line(&s);
            }
        };
        let out = self
            .executor
            .spawn_oneshot_streaming(prompt, &flags, &mut summarize)?;
        parse_stream(&out)
    }

    /// Espelha os docs descobertos, organizados em `docs/<categoria>/<nome>`.
    /// Snapshot: sobrescreve a cada ingest. Docs cuja origem já está no docs_dir
    /// são *movidos* para a categoria certa (organização in-place, sem duplicar).
    /// Devolve quantos foram espelhados.
    fn import_docs(&self, docs: &[ProposedDoc]) -> usize {
        let docs_root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let mut count = 0;
        for d in docs {
            let Some(src) = self.resolve_doc(&d.path) else {
                continue;
            };
            let dest = self.dest_for(d, &src);
            // já está exatamente no destino: nada a fazer.
            if fs::canonicalize(&dest).ok().as_ref() == Some(&src) {
                count += 1;
                continue;
            }
            let copied = dest
                .parent()
                .map(|p| fs::create_dir_all(p).is_ok())
                .unwrap_or(false)
                && fs::copy(&src, &dest).is_ok();
            if !copied {
                continue;
            }
            count += 1;
            // origem solta dentro do docs_dir -> move (remove o original).
            if src.starts_with(&docs_root) {
                let _ = fs::remove_file(&src);
            }
        }
        prune_empty_dirs(&self.root);
        count
    }

    /// Resolve um caminho devolvido pelo agente para um arquivo real no disco
    /// (absoluto, ou relativo ao docs_dir / a algum repo).
    fn resolve_doc(&self, raw: &str) -> Option<PathBuf> {
        let p = PathBuf::from(raw);
        let mut candidates = Vec::new();
        if p.is_absolute() {
            candidates.push(p.clone());
        } else {
            candidates.push(self.root.join(&p));
            for d in &self.add_dirs {
                candidates.push(d.join(&p));
            }
        }
        candidates
            .into_iter()
            .find_map(|c| fs::canonicalize(&c).ok())
            .filter(|c| c.is_file())
    }

    /// Destino organizado: `docs_dir/<categoria>/<nome-canônico>.md`.
    fn dest_for(&self, doc: &ProposedDoc, src: &Path) -> PathBuf {
        self.root.join(folder_for(&doc.kind)).join(dest_name(doc, src))
    }

    /// Varre, espelha os docs e materializa os stubs (com logs ao vivo).
    pub fn run_logged(&self, on_line: &mut dyn FnMut(&str)) -> Result<IngestOutcome> {
        let scan = self.run_scan_logged(&build_prompt(), on_line)?;
        self.finish(scan, Some(on_line))
    }

    /// Captura investigada (dica do usuário) com logs ao vivo.
    pub fn capture_logged(
        &self,
        hint: &str,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<IngestOutcome> {
        let scan = self.run_scan_logged(&capture_prompt(hint), on_line)?;
        self.finish(scan, Some(on_line))
    }

    /// Varre, espelha os docs e materializa os stubs no `.backlog/`.
    pub fn run(&self) -> Result<IngestOutcome> {
        let scan = self.scan()?;
        self.finish(scan, None)
    }

    /// Captura investigada: o claude investiga o projeto a partir de uma dica e
    /// materializa as tasks que endereçam aquilo (ex.: bugfix de UI).
    pub fn capture(&self, hint: &str) -> Result<IngestOutcome> {
        let scan = self.run_scan(&capture_prompt(hint))?;
        self.finish(scan, None)
    }

    /// Etapa final comum: espelha docs e cria stubs (deduplicados).
    fn finish(
        &self,
        scan: ProposedScan,
        on_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<IngestOutcome> {
        let docs_imported = self.import_docs(&scan.docs);
        if let Some(on_line) = on_line {
            on_line(&format!("{docs_imported} doc(s) espelhados em docs/"));
        }
        let created = create_stubs(self.store, &scan.tasks)?;
        Ok(IngestOutcome {
            created,
            docs_imported,
        })
    }
}

/// Prompt da captura investigada (dica do usuário -> task(s) bem-formada(s)).
fn capture_prompt(hint: &str) -> String {
    format!(
        "O usuário observou o seguinte sobre ESTE projeto:\n\n\"{hint}\"\n\n\
Investigue o projeto (docs e código disponíveis) e escreva a(s) task(s) de \
backlog que endereçam isso — normalmente UMA. Para cada uma:\n\
- \"type\": \"impl\" (correção/implementação) ou \"spike\" (investigação que gera doc).\n\
- \"objetivo\": 1-3 frases concretas do que precisa ser feito.\n\
- \"criterio\": itens de aceite verificáveis.\n\
- \"rfcs\"/\"adrs\": só se houver doc relacionado.\n\
Em \"docs\", liste os documentos relevantes que consultou (path absoluto, kind pelo \
conteúdo e um name canônico .md).\n\
Seja específico e fundamentado no que você encontrou. NÃO escreva nem modifique \
arquivos. Retorne pela saída estruturada."
    )
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

/// Extrai tasks + docs do envelope JSON do `claude --output-format json` (campo
/// `structured_output`, validado pelo `--json-schema`).
pub fn parse_structured(out: &str) -> Result<ProposedScan> {
    let v: Value = serde_json::from_str(out.trim())
        .context("parseando a saída JSON do claude (--output-format json)")?;
    parse_envelope(&v)
}

/// Extrai tasks + docs de um envelope `result` (json ou o último evento do stream).
fn parse_envelope(v: &Value) -> Result<ProposedScan> {
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
    let tasks_val = so
        .get("tasks")
        .context("`structured_output` sem campo `tasks`")?;
    let tasks =
        serde_json::from_value(tasks_val.clone()).context("desserializando as tasks propostas")?;
    let docs = match so.get("docs") {
        Some(d) => serde_json::from_value(d.clone()).context("desserializando os docs propostos")?,
        None => Vec::new(),
    };
    Ok(ProposedScan { tasks, docs })
}

/// Extrai tasks + docs da saída `stream-json`: uma linha = um evento JSON; o
/// último evento `type:"result"` traz o envelope com `structured_output`.
pub fn parse_stream(out: &str) -> Result<ProposedScan> {
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

/// Resume um evento `stream-json` em zero, uma ou várias linhas de log legíveis.
/// Usado para os logs ao vivo do ingest/captura. Cada bloco vira sua própria
/// linha (em vez de juntar tudo numa só), e tool calls ganham o seu argumento
/// principal (arquivo, comando, padrão, subagent…).
pub fn summarize_event(line: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            let mut s = "sessão iniciada".to_string();
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                s.push_str(&format!(" · modelo {m}"));
            }
            vec![s]
        }
        Some("assistant") => {
            let model = v
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str);
            let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            else {
                return Vec::new();
            };
            let mut out: Vec<String> = Vec::new();
            for b in content {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        out.push(format!("→ {}", tool_call(name, b.get("input"))));
                    }
                    Some("thinking") => {
                        if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                            let t = t.trim();
                            if !t.is_empty() {
                                out.push(format!("∴ {}", one_line(t)));
                            }
                        }
                    }
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            let t = t.trim();
                            if !t.is_empty() {
                                let prefix = model.map(|m| format!("[{m}] ")).unwrap_or_default();
                                out.push(format!("{prefix}{}", one_line(t)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        Some("result") => {
            let mut s = "concluído".to_string();
            if let (Some(d), Some(c)) = (
                v.get("duration_ms").and_then(Value::as_u64),
                v.get("total_cost_usd").and_then(Value::as_f64),
            ) {
                s.push_str(&format!(" · {:.1}s · ${c:.4}", d as f64 / 1000.0));
            }
            vec![s]
        }
        _ => Vec::new(),
    }
}

/// Junta um texto multi-linha numa linha só (o overlay faz wrap depois).
fn one_line(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Descreve uma tool call com seu argumento principal: `Read <arquivo>`,
/// `Bash <comando>`, `Grep <padrão>`, `Task (Explore) <descrição>`, etc.
fn tool_call(name: &str, input: Option<&Value>) -> String {
    let get = |k: &str| -> Option<String> {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(|s| one_line(s.trim()))
            .filter(|s| !s.is_empty())
    };
    let arg = match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => get("file_path").or_else(|| get("notebook_path")),
        "Bash" | "BashOutput" => get("command").or_else(|| get("description")),
        "Grep" => {
            let pat = get("pattern").unwrap_or_default();
            match get("path") {
                Some(p) => Some(format!("{pat}  em {p}")),
                None => (!pat.is_empty()).then_some(pat),
            }
        }
        "Glob" => get("pattern"),
        "LS" => get("path"),
        "WebFetch" => get("url"),
        "WebSearch" => get("query"),
        "Task" | "Agent" => {
            let sub = get("subagent_type");
            let desc = get("description").or_else(|| get("prompt")).unwrap_or_default();
            Some(match sub {
                Some(s) => format!("({s}) {desc}"),
                None => desc,
            })
        }
        _ => None,
    };
    match arg.filter(|a| !a.is_empty()) {
        Some(a) => format!("{name} {a}"),
        None => name.to_string(),
    }
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
            },
            "docs": {
                "type": "array",
                "description": "Todo documento de design/requisito/decisão encontrado (RFCs, ADRs, PRDs, specs, design docs), já classificado.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "kind", "name"],
                    "properties": {
                        "path": { "type": "string", "description": "Caminho ABSOLUTO do arquivo de origem." },
                        "kind": { "type": "string", "enum": ["rfc", "adr", "prd", "design", "spec", "other"], "description": "Categoria, decidida pelo CONTEÚDO do doc (não pelo nome do arquivo)." },
                        "name": { "type": "string", "description": "Nome canônico padronizado em kebab-case, com ID em maiúsculas quando houver e extensão .md (ex.: ADR-0001-protocolo-como-anotacao.md, RFC-0005-lowering-codegen.md, PRD-03-typechecker.md)." }
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

Além das tasks, preencha "docs" com CADA documento de design/requisito/decisão que você encontrou (todos os RFCs, ADRs, PRDs, specs e design docs — não só os que viraram task). Para cada um:
- "path": o caminho ABSOLUTO do arquivo de origem.
- "kind": a categoria, decidida pelo CONTEÚDO do documento, não pelo nome do arquivo (um arquivo "0001-foo.md" pode na verdade ser um ADR).
- "name": um nome canônico padronizado em kebab-case, com o ID em maiúsculas quando houver e extensão .md (ex.: ADR-0001-protocolo-como-anotacao.md, RFC-0005-lowering-codegen.md, PRD-03-typechecker.md). Seja consistente na numeração e na nomenclatura entre docs da mesma categoria.
O jaum vai espelhar e organizar esses arquivos em docs/<categoria>/ para visualização.

NÃO escreva nem modifique nenhum arquivo. Apenas leia. Retorne o resultado pela saída estruturada (structured output)."#
        .to_string()
}
