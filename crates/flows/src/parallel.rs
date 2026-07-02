//! Parallelism analysis: uses the LLM to decide which tasks REALLY collide
//! (same files/area in the same repo), not just "share a repo". Read-only;
//! produces an INTERNAL report consumed by the Board — never writes to the repo.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor};
use jaum_core::{Status, Store, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ingest::summarize_event;

/// A conflict between two tasks: they overlap (same files/area) and must not
/// run in parallel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub a: String,
    pub b: String,
    pub repo: String,
    pub reason: String,
}

/// Parallelism report: the colliding pairs. Tasks with no edge between them
/// can run in parallel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParallelReport {
    pub conflicts: Vec<Conflict>,
}

impl ParallelReport {
    /// The conflict between two tasks (in either order), if any.
    pub fn conflict_between(&self, x: &str, y: &str) -> Option<&Conflict> {
        self.conflicts
            .iter()
            .find(|c| (c.a == x && c.b == y) || (c.a == y && c.b == x))
    }
}

/// Analysis orchestrator. Generic over the executor (testable with a fake).
pub struct Parallel<'a, E: Executor> {
    store: &'a Store,
    executor: &'a E,
    /// cwd of the scan (project or docs root); just needs to be a valid dir.
    root: PathBuf,
    /// Repos opened for reading (`--add-dir`), where the analysis inspects the code.
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

    /// Read-only flags + read access to the repos + structured output.
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
            extra.extend(
                self.repos
                    .values()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
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

    /// Analyzes the open (non-merged) tasks and returns the colliding pairs.
    /// `on_line` receives a readable summary of each event (live logs).
    pub fn analyze_logged(&self, on_line: &mut dyn FnMut(&str)) -> Result<ParallelReport> {
        let open: Vec<Task> = self
            .store
            .list(None)?
            .into_iter()
            .filter(|t| t.status != Status::Merged)
            .collect();
        // fewer than two tasks: nothing to compare.
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

/// Extracts the conflicts from `stream-json` output: the last `result` event
/// carries the envelope with `structured_output`.
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
    let v = last_result.context("stream-json has no final `result` event")?;
    parse_envelope(&v)
}

/// Extracts the conflicts from a `result` envelope (json or last stream event).
pub fn parse_structured(out: &str) -> Result<ParallelReport> {
    let v: Value = serde_json::from_str(out.trim())
        .context("parsing claude's JSON output (--output-format json)")?;
    parse_envelope(&v)
}

fn parse_envelope(v: &Value) -> Result<ParallelReport> {
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "claude reported an error: {}",
            v.get("result").and_then(Value::as_str).unwrap_or("unknown")
        );
    }
    let so = v
        .get("structured_output")
        .context("output has no `structured_output` (was --json-schema applied?)")?;
    let conflicts = match so.get("conflicts") {
        Some(c) => serde_json::from_value(c.clone()).context("deserializing the conflicts")?,
        None => Vec::new(),
    };
    Ok(ParallelReport { conflicts })
}

/// Schema of the structured output.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["conflicts"],
        "properties": {
            "conflicts": {
                "type": "array",
                "description": "Only the PAIRS of tasks that actually collide (same files/area in the same repo). Anything not listed here can run in parallel.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["a", "b", "repo", "reason"],
                    "properties": {
                        "a": { "type": "string", "description": "id of the first task (e.g. TASK-002)" },
                        "b": { "type": "string", "description": "id of the second task" },
                        "repo": { "type": "string", "description": "slug of the repo where they collide (e.g. owner/name)" },
                        "reason": { "type": "string", "description": "Why they collide, short and concrete (e.g. both edit src/render.rs)." }
                    }
                }
            }
        }
    })
}

/// Analysis prompt: lists the open tasks and asks for the colliding pairs.
pub fn build_prompt(tasks: &[Task]) -> String {
    let mut p = String::new();
    p.push_str(
        "You analyze which tasks can run IN PARALLEL without conflict. Two tasks \
conflict when, once implemented, they would touch the SAME files or the same \
area of the code (in the same repository). Tasks in different repositories NEVER \
conflict. Being in the same repo is NOT a conflict by itself: they only conflict if \
their areas genuinely overlap.\n\n\
Inspect the repositories (read-only) to confirm where each task would touch. \
Be precise: when in doubt, assume they do NOT conflict (parallel is the default). Report \
only the colliding pairs.\n\n## Open tasks\n\n",
    );
    for t in tasks {
        let repos = t.linked_repos().join(", ");
        p.push_str(&format!(
            "### {} (repos: {})\n",
            t.id,
            if repos.is_empty() {
                "none".into()
            } else {
                repos
            }
        ));
        let body = t.body.trim();
        if !body.is_empty() {
            p.push_str(body);
            p.push('\n');
        }
        p.push('\n');
    }
    p
}
