//! LLM ingest: `claude -p` (headless, read-only) scans the project, finds the
//! documents (RFC/ADR/PRD/spec/design — however the project names and organizes
//! them) and returns JSON stubs validated by schema. jaum (and only jaum) writes
//! the `.backlog/`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor};
use jaum_core::{Store, Task, TaskType};
use serde::Deserialize;
use serde_json::{Value, json};

/// Raw result of a scan: proposed tasks + documents found (already classified by
/// the agent, for jaum to mirror organized under `docs/`).
#[derive(Debug, Default)]
pub struct ProposedScan {
    pub tasks: Vec<ProposedTask>,
    pub docs: Vec<ProposedDoc>,
}

/// A discovered document: source path + the classification the agent gave by
/// reading the CONTENT (not the file name) and a standardized canonical name.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedDoc {
    /// Absolute path of the source file (in the repo or in docs_dir).
    pub path: String,
    /// Category: rfc | adr | prd | design | spec | other.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Proposed canonical name (e.g. `ADR-0001-protocol-as-annotation.md`).
    #[serde(default)]
    pub name: String,
}

fn default_kind() -> String {
    "other".to_string()
}

/// Destination folder for each doc category.
fn folder_for(kind: &str) -> &'static str {
    match kind.to_lowercase().as_str() {
        "rfc" | "rfcs" => "rfcs",
        "adr" | "adrs" => "adrs",
        "prd" | "prds" => "prd",
        "design" => "design",
        "spec" | "specs" => "specs",
        _ => "other",
    }
}

/// Canonical destination name: uses the proposed `name` (basename only, with `.md`),
/// falling back to the original file name when the agent didn't propose one.
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

/// Removes empty directories under `root` (not `root` itself). Cleans up folders
/// left orphaned after moving docs into their categories.
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

/// Final result of an ingest/capture: stubs created + docs copied.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    pub created: Vec<Task>,
    pub docs_imported: usize,
}

/// A task proposed by the agent (before it becomes backlog on disk).
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
            "## Objective\n\n{}\n\n## Acceptance criteria\n",
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

    /// Dedup key: refs normalized and sorted.
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

/// Ingest orchestrator.
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

    /// Builds the `--add-dir <repos>` common to both scan forms.
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

    /// Read-only scan flags (the agent reads and reports; jaum is what writes).
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

    /// Runs a scan prompt via `claude -p` (read-only, structured output).
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

    /// Scans the docs and returns the proposed tasks.
    pub fn scan(&self) -> Result<ProposedScan> {
        self.run_scan(&build_prompt())
    }

    /// Like `run_scan`, but streaming: uses `stream-json` and forwards a readable
    /// summary of each event to `on_line` while claude works.
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
        // the executor delivers raw lines (JSON); we summarize them to friendly text.
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

    /// Mirrors the discovered docs, organized under `docs/<category>/<name>`.
    /// Snapshot: overwrites on every ingest. Docs whose source is already in
    /// docs_dir are *moved* to the right category (in-place organization, no
    /// duplication). Returns how many were mirrored.
    fn import_docs(&self, docs: &[ProposedDoc]) -> usize {
        let docs_root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let mut count = 0;
        for d in docs {
            let Some(src) = self.resolve_doc(&d.path) else {
                continue;
            };
            let dest = self.dest_for(d, &src);
            // already exactly at the destination: nothing to do.
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
            // source loose inside docs_dir -> move (remove the original).
            if src.starts_with(&docs_root) {
                let _ = fs::remove_file(&src);
            }
        }
        prune_empty_dirs(&self.root);
        count
    }

    /// Resolves a path returned by the agent to a real file on disk (absolute, or
    /// relative to docs_dir / some repo).
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

    /// Organized destination: `docs_dir/<category>/<canonical-name>.md`.
    fn dest_for(&self, doc: &ProposedDoc, src: &Path) -> PathBuf {
        self.root.join(folder_for(&doc.kind)).join(dest_name(doc, src))
    }

    /// Scans, mirrors the docs and materializes the stubs (with live logs).
    pub fn run_logged(&self, on_line: &mut dyn FnMut(&str)) -> Result<IngestOutcome> {
        let scan = self.run_scan_logged(&build_prompt(), on_line)?;
        self.finish(scan, Some(on_line))
    }

    /// Investigated capture (user hint) with live logs.
    pub fn capture_logged(
        &self,
        hint: &str,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<IngestOutcome> {
        let scan = self.run_scan_logged(&capture_prompt(hint), on_line)?;
        self.finish(scan, Some(on_line))
    }

    /// Scans, mirrors the docs and materializes the stubs in `.backlog/`.
    pub fn run(&self) -> Result<IngestOutcome> {
        let scan = self.scan()?;
        self.finish(scan, None)
    }

    /// Investigated capture: claude investigates the project from a hint and
    /// materializes the tasks that address it (e.g. a UI bugfix).
    pub fn capture(&self, hint: &str) -> Result<IngestOutcome> {
        let scan = self.run_scan(&capture_prompt(hint))?;
        self.finish(scan, None)
    }

    /// Common final step: mirrors docs and creates (deduplicated) stubs.
    fn finish(
        &self,
        scan: ProposedScan,
        on_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<IngestOutcome> {
        let docs_imported = self.import_docs(&scan.docs);
        if let Some(on_line) = on_line {
            on_line(&format!("{docs_imported} doc(s) mirrored in docs/"));
        }
        let created = create_stubs(self.store, &scan.tasks)?;
        Ok(IngestOutcome {
            created,
            docs_imported,
        })
    }
}

/// Investigated-capture prompt (user hint -> well-formed task(s)).
fn capture_prompt(hint: &str) -> String {
    format!(
        "The user observed the following about THIS project:\n\n\"{hint}\"\n\n\
Investigate the project (available docs and code) and write the backlog task(s) \
that address it — usually ONE. For each:\n\
- \"type\": \"impl\" (fix/implementation) or \"spike\" (investigation that produces a doc).\n\
- \"objetivo\": 1-3 concrete sentences of what needs to be done.\n\
- \"criterio\": verifiable acceptance items.\n\
- \"rfcs\"/\"adrs\": only if there's a related doc.\n\
In \"docs\", list the relevant documents you consulted (absolute path, kind by \
content and a canonical .md name).\n\
Be specific and grounded in what you found. Do NOT write or modify \
files. Return through the structured output."
    )
}

/// Creates the stubs in the store, deduplicating by the set of already-existing refs.
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
            continue; // backlog already exists for these refs
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

/// Extracts tasks + docs from the `claude --output-format json` envelope (field
/// `structured_output`, validated by `--json-schema`).
pub fn parse_structured(out: &str) -> Result<ProposedScan> {
    let v: Value = serde_json::from_str(out.trim())
        .context("parsing claude's JSON output (--output-format json)")?;
    parse_envelope(&v)
}

/// Extracts tasks + docs from a `result` envelope (json or the last stream event).
fn parse_envelope(v: &Value) -> Result<ProposedScan> {
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "claude reported an error: {}",
            v.get("result")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    let so = v
        .get("structured_output")
        .context("output missing `structured_output` (was --json-schema applied?)")?;
    let tasks_val = so
        .get("tasks")
        .context("`structured_output` missing `tasks` field")?;
    let tasks =
        serde_json::from_value(tasks_val.clone()).context("deserializing the proposed tasks")?;
    let docs = match so.get("docs") {
        Some(d) => serde_json::from_value(d.clone()).context("deserializing the proposed docs")?,
        None => Vec::new(),
    };
    Ok(ProposedScan { tasks, docs })
}

/// Extracts tasks + docs from the `stream-json` output: one JSON event per line;
/// the last `type:"result"` event carries the envelope with `structured_output`.
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
    let v = last_result.context("stream-json missing final `result` event")?;
    parse_envelope(&v)
}

/// Summarizes a `stream-json` event into zero, one or several readable log lines.
/// Used for the live logs of ingest/capture. Each block becomes its own line
/// (instead of joining everything into one), and tool calls get their main
/// argument (file, command, pattern, subagent…).
pub fn summarize_event(line: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            let mut s = "session started".to_string();
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                s.push_str(&format!(" · model {m}"));
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
            let mut s = "done".to_string();
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

/// Joins a multi-line text into a single line (the overlay wraps later).
fn one_line(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Describes a tool call with its main argument: `Read <file>`, `Bash <command>`,
/// `Grep <pattern>`, `Task (Explore) <description>`, etc.
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
                Some(p) => Some(format!("{pat}  in {p}")),
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

/// Schema of the structured output required from the agent.
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
                "description": "Every design/requirement/decision document found (RFCs, ADRs, PRDs, specs, design docs), already classified.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "kind", "name"],
                    "properties": {
                        "path": { "type": "string", "description": "ABSOLUTE path of the source file." },
                        "kind": { "type": "string", "enum": ["rfc", "adr", "prd", "design", "spec", "other"], "description": "Category, decided by the CONTENT of the doc (not by the file name)." },
                        "name": { "type": "string", "description": "Standardized canonical name in kebab-case, with the ID uppercased when present and a .md extension (e.g. ADR-0001-protocol-as-annotation.md, RFC-0005-lowering-codegen.md, PRD-03-typechecker.md)." }
                    }
                }
            }
        }
    })
}

/// Ingest agent prompt.
pub fn build_prompt() -> String {
    r#"You are initializing the task backlog for the `jaum` tool by scanning THIS project.

Goal: find ALL design, requirement or decision documents — RFCs, ADRs, PRDs, specs, design docs, proposals — no matter how this project names or organizes them (any folder, any naming convention, upper or lower case, with or without frontmatter). Look in the project root and in every available repository.

For each coherent unit of implementation work implied by those documents, generate a backlog task stub. Rules:
- "type": "impl" for implementation work; "spike" for exploratory work that should produce a document (RFC/ADR) instead of a PR.
- "rfcs"/"adrs": the IDs of the documents that motivate the task, using the project's own convention (e.g. RFC-0001, PRD-05, ADR-3). Normalize to uppercase.
- "objetivo": 1 to 3 sentences describing the goal.
- "criterio": a few acceptance-criteria items.
- Be conservative: one task per real unit of work; do NOT explode each document into dozens of tasks. Prefer ~1 task per PRD/RFC, unless it clearly contains independent deliverables.
- ADRs are decisions/records; only turn one into a task if it implies concrete pending work.

Beyond the tasks, fill "docs" with EVERY design/requirement/decision document you found (all RFCs, ADRs, PRDs, specs and design docs — not just the ones that became tasks). For each:
- "path": the ABSOLUTE path of the source file.
- "kind": the category, decided by the CONTENT of the document, not the file name (a file "0001-foo.md" may actually be an ADR).
- "name": a standardized canonical name in kebab-case, with the ID uppercased when present and a .md extension (e.g. ADR-0001-protocol-as-annotation.md, RFC-0005-lowering-codegen.md, PRD-03-typechecker.md). Be consistent in numbering and naming across docs of the same category.
jaum will mirror and organize these files under docs/<category>/ for viewing.

Do NOT write or modify any file. Only read. Return the result through the structured output."#
        .to_string()
}
