//! Review phase: the semantic gate. Opens a read-only session with full context
//! (ALL RFCs/ADRs + PR diffs + checklist of `enforce: review` constraints),
//! records the report line by line and decides `is_clean`.
//!
//! Detective guarantee (mandatory): `is_clean` only passes with ZERO findings AND
//! every `enforce: review` constraint marked `ok` — the semantic side.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use jaum_adapters::{ExecFlags, Executor, Gh, Git, Session};
use jaum_core::{Enforce, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Current Unix time in seconds (0 if the clock predates the epoch).
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Verdict of a semantic constraint in the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintVerdict {
    /// Not yet evaluated — `is_clean` fails while any item is pending.
    Pending,
    Ok,
    Failed,
}

/// Result of checking an `enforce: review` constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintResult {
    pub text: String,
    pub verdict: ConstraintVerdict,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Finding severity. Only `Blocker`/`Major` fail the review; `Minor`/`Nit` are
/// informational (they don't hold the review in DIRTY state).
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

/// Conservative default: if the agent omits it, treat as `major` (fails) so a
/// problem isn't hidden by forgetfulness.
fn default_severity() -> Severity {
    Severity::Major
}

/// A review finding, anchored at file:line and (ideally) to an RFC/ADR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
    /// Violated RFC/ADR, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default = "default_severity")]
    pub severity: Severity,
}

impl Finding {
    /// Does this finding fail the review?
    pub fn is_blocking(&self) -> bool {
        self.severity.blocks()
    }

    /// Line "[SEV] file:line - message [(ref)]".
    pub fn render(&self) -> String {
        let loc = match self.line {
            Some(l) => format!("{}:{}", self.file, l),
            None => self.file.clone(),
        };
        let tag = self.severity.tag();
        match &self.reference {
            Some(r) => format!("[{tag}] {loc} - {} (violates {r})", self.message),
            None => format!("[{tag}] {loc} - {}", self.message),
        }
    }
}

/// Report persisted at `.backlog/TASK-NNN.review.md` (structured frontmatter +
/// readable body). The source of truth for the review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub task: String,
    /// Unix time (seconds) the capture was written. Absent on reports produced
    /// before this was recorded; the UI just omits the "when" then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<u64>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub constraints: Vec<ConstraintResult>,
    /// Verdict for each acceptance criterion of the task (same shape as constraints).
    #[serde(default)]
    pub criteria: Vec<ConstraintResult>,
}

impl ReviewReport {
    /// Clean if there is NO blocking finding (blocker/major), every `enforce: review`
    /// constraint is `ok` AND every acceptance criterion was met (`ok`). `minor`/`nit`
    /// findings show up but don't fail (otherwise a nitpick would hold the review in
    /// DIRTY forever).
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

    /// Number of items still failing/pending: constraints + non-`ok` criteria.
    pub fn unmet_count(&self) -> usize {
        self.constraints
            .iter()
            .chain(self.criteria.iter())
            .filter(|c| c.verdict != ConstraintVerdict::Ok)
            .count()
    }

    /// Number of blocking findings (blocker/major).
    pub fn blocking_count(&self) -> usize {
        self.findings.iter().filter(|f| f.is_blocking()).count()
    }

    /// Renders the readable markdown body (line by line).
    fn render_body(&self) -> String {
        let mut b = format!("# Review {}\n\n", self.task);
        b.push_str(&format!(
            "**Result:** {}\n\n",
            if self.is_clean() { "CLEAN" } else { "DIRTY" }
        ));

        b.push_str("## Findings\n");
        if self.findings.is_empty() {
            b.push_str("- (none)\n");
        } else {
            for f in &self.findings {
                b.push_str(&format!("- {}\n", f.render()));
            }
        }

        b.push_str("\n## Constraints (enforce: review)\n");
        render_checklist(&mut b, &self.constraints);

        b.push_str("\n## Acceptance criteria\n");
        render_checklist(&mut b, &self.criteria);
        b
    }
}

/// Read-only flags: no writes. A read-tool whitelist is the strongest guarantee
/// that the review mutates nothing.
pub fn read_only_flags() -> ExecFlags {
    ExecFlags::new()
        .with_disallowed(["Edit", "Write", "NotebookEdit", "Bash"])
        .with_model(crate::AGENT_MODEL)
}

/// Review phase orchestrator.
pub struct Review<'a, E: Executor> {
    store: &'a Store,
    git: &'a Git,
    gh: &'a Gh,
    executor: &'a E,
    docs_dir: PathBuf,
    /// Explicit mapping slug "owner/name" -> local repo path.
    repos: HashMap<String, PathBuf>,
    /// Project conventions (conventions.md).
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

    /// cwd for the review session: the task's linked repo (first), else the
    /// project's first repo, else docs_dir. So the agent opens in the code, not in
    /// the jaum home.
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

    /// Read-only flags + cwd in the repo + read access to all repos.
    fn review_flags(&self, id: &str) -> ExecFlags {
        let mut flags = read_only_flags();
        flags.cwd = Some(self.review_cwd(id));
        if !self.repos.is_empty() {
            flags.extra.push("--add-dir".to_string());
            flags.extra.extend(
                self.repos
                    .values()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
        }
        flags
    }

    /// Mandatory checklist: each `enforce: review` constraint becomes an item to
    /// validate, initially `pending`. `is_clean` fails while any is pending.
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

    /// Mandatory checklist of the task's acceptance criteria (from the body), each
    /// initially `pending`. `is_clean` fails while any is not `ok`.
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

    /// Full review context: ALL project RFCs/ADRs + the task PR diffs + the
    /// `enforce: review` constraints as a mandatory checklist.
    pub fn build_context(&self, id: &str) -> Result<String> {
        let task = self.store.get(id)?;
        let mut c = String::new();
        c.push_str(&format!("# Read-only review of {}\n\n", task.id));
        c.push_str("You are the reviewer. Do NOT change anything. Report findings as `file:line — what it violates (RFC/ADR)`.\n\n");

        // task objective (the body, which holds the objective and acceptance criteria)
        let body = task.body.trim();
        if !body.is_empty() {
            c.push_str("## What the task asks for\n\n");
            c.push_str(body);
            c.push_str("\n\n");
        }

        // all project docs
        c.push_str("## Project RFCs/ADRs\n\n");
        let docs = collect_docs(&self.docs_dir)?;
        if docs.is_empty() {
            c.push_str("(no docs found in ");
            c.push_str(&self.docs_dir.to_string_lossy());
            c.push_str(")\n\n");
        } else {
            for (name, body) in &docs {
                c.push_str(&format!("### {name}\n{body}\n\n"));
            }
        }

        // PR diffs — prefer the GitHub PR (via gh); fall back to the local diff
        // when there's no open PR yet.
        c.push_str("## PR diffs\n\n");
        for link in &task.prs {
            let repo_dir = self.repo_path(&link.repo);
            // resolve the PR number (the saved one, or discovered from the branch
            // via gh, running in the repo directory).
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
                        c.push_str(&format!(
                            "### {} PR #{pr}: {title}\n\n{body}\n\n",
                            link.repo
                        ));
                    }
                    let diff = self
                        .gh
                        .pr_diff(dir, pr)
                        .unwrap_or_else(|e| format!("(could not get diff for PR #{pr}: {e})"));
                    c.push_str(&format!(
                        "#### diff of PR #{pr} ({} @ {})\n```diff\n{}\n```\n\n",
                        link.repo,
                        link.branch,
                        diff.trim()
                    ));
                }
                _ => {
                    // no PR yet: local branch diff (fallback).
                    let diff = match &repo_dir {
                        Some(repo_path) => {
                            self.git.diff(repo_path, &link.branch).unwrap_or_else(|e| {
                                format!("(could not get diff for {}: {e})", link.repo)
                            })
                        }
                        None => format!("(repo {} not mapped in the project)", link.repo),
                    };
                    c.push_str(&format!(
                        "### {} @ {} (no PR yet - local diff)\n```diff\n{}\n```\n\n",
                        link.repo,
                        link.branch,
                        diff.trim()
                    ));
                }
            }
        }

        // where the code lives, to read beyond the diff
        c.push_str("## Working tree of the repos (read access granted)\n\n");
        if self.repos.is_empty() {
            c.push_str("(no repos mapped in the project)\n\n");
        } else {
            let mut entries: Vec<_> = self.repos.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (slug, path) in entries {
                c.push_str(&format!("- {slug}: `{}`\n", path.display()));
            }
            c.push_str(
                "\nYou MAY read these directories with Read/Grep/Glob (access granted) for context \
beyond the diff: confirm that cited files/fixtures exist, see unchanged code and check \
signatures. The working tree may be on the base branch; the DIFF above is the source of truth for what \
changed in the PR. There is NO Bash here (review is read-only) — do not run tests or commands, only read.\n\n",
            );
        }

        // project conventions (always checked)
        let conv = self.conventions.trim();
        if !conv.is_empty() {
            c.push_str("## Project conventions to respect\n\n");
            c.push_str(conv);
            c.push_str("\n\n");
        }

        // mandatory checklist of semantic constraints
        c.push_str("## Constraints to validate (enforce: review) — MANDATORY\n\n");
        let checklist = self.check_semantic_constraints(id)?;
        if checklist.is_empty() {
            c.push_str("(none)\n");
        } else {
            for item in &checklist {
                c.push_str(&format!("- [ ] {}\n", item.text));
            }
        }

        // mandatory checklist of acceptance criteria (from the task body)
        c.push_str(
            "\n## Acceptance criteria to validate — MANDATORY\n\n\
Looking at the DIFF, confirm whether what changed actually MEETS each criterion below. \
Mark `ok` only if the diff satisfies the criterion; `failed` if it doesn't; `pending` if \
the diff can't tell.\n\n",
        );
        let criteria = self.acceptance_checklist(id)?;
        if criteria.is_empty() {
            c.push_str("(none)\n");
        } else {
            for item in &criteria {
                c.push_str(&format!("- [ ] {}\n", item.text));
            }
        }
        Ok(c)
    }

    /// Opens the read-only review session with the full context. Returns the
    /// session and the claude UUID (`--session-id`), to resume later.
    pub fn start(&self, id: &str) -> Result<(Session, String)> {
        let context = self.build_context(id)?;
        let claude_session_id = uuid::Uuid::new_v4().to_string();
        let flags = self.review_flags(id).with_session_id(&claude_session_id);
        let session = self.executor.spawn_interactive(&context, &flags)?;
        Ok((session, claude_session_id))
    }

    /// Resumes a read-only review session with `--resume`, without resending the
    /// context (claude reloads the conversation). `cwd` must match the origin.
    pub fn resume(&self, id: &str, uuid: &str, cwd: &Path) -> Result<Session> {
        let mut flags = self.review_flags(id);
        flags.cwd = Some(cwd.to_path_buf());
        flags = flags.with_resume(uuid);
        self.executor.spawn_interactive("", &flags)
    }

    /// Structured capture of the review: runs `claude -p` read-only with the full
    /// context + schema, turns the output into `findings` + verdicts and WRITES the
    /// `.review.md`. jaum guarantees the structure: the constraint list is canonical
    /// (`enforce: review`); claude only supplies each verdict.
    /// `on_line` receives the live logs (a summary of the stream events).
    pub fn capture_logged(&self, id: &str, on_line: &mut dyn FnMut(&str)) -> Result<ReviewReport> {
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
            extra.extend(
                self.repos
                    .values()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
        }
        let mut flags = read_only_flags();
        flags.cwd = Some(self.review_cwd(id));
        flags.extra = extra;

        // reuse the ingest event summary for the live logs.
        let mut summarize = |raw: &str| {
            for s in crate::ingest::summarize_event(raw) {
                on_line(&s);
            }
        };
        let out = self
            .executor
            .spawn_oneshot_streaming(&prompt, &flags, &mut summarize)?;

        let (findings, prop_constraints, prop_criteria) = parse_review_stream(&out)?;

        // the constraint and criteria lists are canonical (from the task); claude
        // only fills in the verdict. An unmentioned item stays `pending`.
        let constraints = merge_verdicts(self.check_semantic_constraints(id)?, &prop_constraints);
        let criteria = merge_verdicts(self.acceptance_checklist(id)?, &prop_criteria);

        let report = ReviewReport {
            task: id.to_string(),
            reviewed_at: Some(now_unix_secs()),
            findings,
            constraints,
            criteria,
        };
        self.write_report(&report)?;
        on_line(&format!(
            "review written: {} finding(s), {}",
            report.findings.len(),
            if report.is_clean() { "CLEAN" } else { "DIRTY" }
        ));
        Ok(report)
    }

    /// Writes the report to `.backlog/TASK-NNN.review.md`.
    pub fn write_report(&self, report: &ReviewReport) -> Result<()> {
        let path = self.store.review_path(&report.task);
        self.store.write_doc(&path, report, &report.render_body())
    }

    /// Loads the persisted report.
    pub fn load_report(&self, id: &str) -> Result<ReviewReport> {
        let path = self.store.review_path(id);
        let (report, _body) = self.store.read_doc::<ReviewReport>(&path)?;
        Ok(report)
    }

    /// `true` only if the persisted report is clean (zero findings AND every
    /// `enforce: review` ok).
    pub fn is_clean(&self, id: &str) -> Result<bool> {
        Ok(self.load_report(id)?.is_clean())
    }

    /// Injects the findings into the same task's play session (dirty handoff).
    pub fn handoff(&self, id: &str, session: &mut Session) -> Result<()> {
        let report = self.load_report(id)?;
        session.write_line(&handoff_message(&report))
    }
}

/// Handoff message: the review's findings + failed constraints, to inject into a
/// play session to fix. (Pure function, reused by the TUI.)
pub fn handoff_message(report: &ReviewReport) -> String {
    let mut msg = String::from("Review found issues, please fix:\n");
    for f in &report.findings {
        msg.push_str(&format!("- {}\n", f.render()));
    }
    for c in &report.constraints {
        if c.verdict == ConstraintVerdict::Failed {
            msg.push_str(&format!("- failed constraint: {} — {}\n", c.text, c.note));
        }
    }
    for c in &report.criteria {
        if c.verdict != ConstraintVerdict::Ok {
            msg.push_str(&format!("- unmet criterion: {} — {}\n", c.text, c.note));
        }
    }
    msg
}

/// Matches the canonical list (from the task) with the verdicts proposed by claude,
/// keyed by text. An unmentioned item stays as it came (`pending`).
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

/// Renders a checklist of verdicts (constraints or criteria) in the body.
fn render_checklist(b: &mut String, items: &[ConstraintResult]) {
    if items.is_empty() {
        b.push_str("- (none)\n");
        return;
    }
    for c in items {
        let tag = match c.verdict {
            ConstraintVerdict::Ok => "OK",
            ConstraintVerdict::Failed => "REJECTED",
            ConstraintVerdict::Pending => "PENDING",
        };
        if c.note.is_empty() {
            b.push_str(&format!("- [{tag}] {}\n", c.text));
        } else {
            b.push_str(&format!("- [{tag}] {} - {}\n", c.text, c.note));
        }
    }
}

/// Instruction appended to the context for the structured review capture.
const REVIEW_INSTRUCTION: &str = "Review the diff above against the RFCs/ADRs, the conventions and the \
constraint checklist. Be EXHAUSTIVE in this single pass: report ALL the problems you \
find at once (don't go bit by bit), from bugs and violations to improvements. Return ONLY through the \
structured output: `findings` (each with `file`, `line` when known, `message`, `severity` and \
`reference` to the violated RFC/ADR when applicable), `constraints` (ONE entry per item of the \
constraint checklist) and `criteria` (ONE entry per item of the acceptance-criteria checklist), each \
with a `verdict` of `ok`/`failed`/`pending` and a short `note`. \
Classify each finding with `severity`: `blocker` (broken/incorrect, must be fixed), `major` \
(important), `minor` (improvement) or `nit` (cosmetic). Only `blocker` and `major` fail the review; \
`minor`/`nit` are informational. Repeat the text of each constraint and each criterion EXACTLY as it \
appears in the checklist. For the acceptance criteria, decide by looking at the DIFF: `ok` if what changed meets \
the criterion, `failed` if it doesn't, `pending` if the diff doesn't allow a decision. \
Write `message` and `note` CONCISELY and directly: one objective sentence, no dashes and no \
fluff. Don't change anything; only read. Be strict: only mark `ok` if it's actually met.";

/// Schema of the review's structured output.
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
                        "verdict": { "type": "string", "enum": ["ok", "failed", "pending"] },
                        "note": { "type": "string" }
                    }
                }
            },
            "criteria": {
                "type": "array",
                "description": "One verdict per acceptance criterion in the checklist. Does the diff meet the criterion?",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "verdict"],
                    "properties": {
                        "text": { "type": "string" },
                        "verdict": { "type": "string", "enum": ["ok", "failed", "pending"] },
                        "note": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// Extracts `findings` + `constraints` + `criteria` from the last `result` event.
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
    let v = last.context("stream-json missing final `result` event")?;
    if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "claude reported an error: {}",
            v.get("result").and_then(Value::as_str).unwrap_or("unknown")
        );
    }
    let so = v
        .get("structured_output")
        .context("output missing `structured_output` (was --json-schema applied?)")?;
    let findings = match so.get("findings") {
        Some(f) => serde_json::from_value(f.clone()).context("deserializing findings")?,
        None => Vec::new(),
    };
    let constraints = match so.get("constraints") {
        Some(c) => serde_json::from_value(c.clone()).context("deserializing constraints")?,
        None => Vec::new(),
    };
    let criteria = match so.get("criteria") {
        Some(c) => serde_json::from_value(c.clone()).context("deserializing criteria")?,
        None => Vec::new(),
    };
    Ok((findings, constraints, criteria))
}

/// Reads the RFCs/ADRs (`RFC-*.md`, `ADR-*.md`) from a directory, sorted by name.
fn collect_docs(docs_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if !docs_dir.exists() {
        return Ok(out);
    }
    for entry in
        fs::read_dir(docs_dir).with_context(|| format!("reading docs in {}", docs_dir.display()))?
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
