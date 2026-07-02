use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Task state in the flow. The single source of truth for progress — PR merge
/// is read from `gh`, never mirrored here (only the final `merged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Backlog,
    Ready,
    Wip,
    Review,
    Merged,
}

/// Kind of task. `Spike` produces a document (RFC/ADR); it has no PR and no play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskType {
    Impl,
    Spike,
}

/// How a constraint is enforced (the two core patches):
/// `Hook` = mechanical, blocked preemptively by a PreToolUse hook;
/// `Review` = semantic, checked item by item and mandatory at review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforce {
    Hook,
    Review,
}

/// Task↔PR link. `pr == 0` means "not created yet": the real number is read
/// from `gh` (downstream), never made up by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    pub repo: String,
    #[serde(default)]
    pub pr: u64,
    pub branch: String,
}

/// A "don't do X" directive attached to a task, classified by how it's enforced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub text: String,
    pub enforce: Enforce,
    /// Regex the PreToolUse hook uses to block (`enforce: hook`). When absent,
    /// [`Constraint::hook_pattern`] derives a heuristic from `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

impl Constraint {
    /// Pattern (extended regex, used with `grep -iE` in the hook) to block this
    /// constraint. Uses an explicit `pattern` if present; otherwise derives from
    /// `text`: prefers a path-like token (`src/legacy/`), falling back to the
    /// significant words (dropping stopwords) as an alternation.
    pub fn hook_pattern(&self) -> String {
        if let Some(p) = &self.pattern {
            return p.clone();
        }
        let lower = self.text.to_lowercase();
        // 1) token that looks like a path/file (contains `/` or `.`)
        if let Some(path) = lower
            .split_whitespace()
            .find(|t| t.contains('/') || t.contains('.'))
        {
            return escape_regex(
                path.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.'),
            );
        }
        // 2) significant words
        const STOP: &[&str] = &[
            "not", "don", "the", "and", "for", "that", "with", "without", "keep", "run", "touch",
            "use", "make", "new", "any", "all",
        ];
        let words: Vec<String> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2 && !STOP.contains(w))
            .map(escape_regex)
            .collect();
        if words.is_empty() {
            // fallback: the whole text escaped (never matches empty)
            return escape_regex(lower.trim());
        }
        words.join("|")
    }
}

/// Escapes extended-regex (ERE) metacharacters so they match literally.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// "owner/name" slug of a repository. An alias for now; becomes a newtype if the
/// adapters (git/gh) need more semantics.
pub type Repo = String;

/// Merge state of a PR, **read** from `gh` — never mirrored in the markdown.
/// Basis for `finish`, which updates the task status from here without ever
/// running the merge itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    /// `pr == 0`: not created yet.
    NotCreated,
    /// PR open, not merged yet.
    Open,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
    /// Unrecognized state returned by `gh`.
    Unknown,
}

/// A task from `.backlog/`. Fields without `skip` are exactly the YAML
/// frontmatter; `body` and `path` are filled in by the store after parsing and
/// are not serialized back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rfcs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<String>,

    #[serde(skip)]
    pub body: String,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Task {
    pub fn is_spike(&self) -> bool {
        matches!(self.task_type, TaskType::Spike)
    }

    /// Constraints of a given enforcement class (Hook = mechanical, Review = semantic).
    pub fn constraints_by(&self, kind: Enforce) -> Vec<&Constraint> {
        self.constraints
            .iter()
            .filter(|c| c.enforce == kind)
            .collect()
    }

    /// Acceptance criteria extracted from the body (section whose heading contains
    /// "acceptance"). Each list item (`- [ ] text`, `- [x] text` or `- text`) becomes
    /// a criterion; the empty placeholder is skipped. Used by review as a checklist.
    pub fn acceptance_criteria(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut in_section = false;
        for line in self.body.lines() {
            let t = line.trim();
            if let Some(h) = t.strip_prefix('#') {
                // any other section closes the acceptance one; a new acceptance section (re)opens it.
                in_section = h
                    .trim_start_matches('#')
                    .to_lowercase()
                    .contains("acceptance");
                continue;
            }
            if !in_section {
                continue;
            }
            let Some(rest) = t.strip_prefix('-').or_else(|| t.strip_prefix('*')) else {
                continue;
            };
            let mut item = rest.trim();
            // strip the checkbox `[ ]` / `[x]` if present.
            if let Some(after) = item.strip_prefix('[')
                && let Some((_, tail)) = after.split_once(']')
            {
                item = tail.trim();
            }
            if !item.is_empty() {
                out.push(item.to_string());
            }
        }
        out
    }

    /// Repos linked via PRs, deduplicated preserving order.
    pub fn linked_repos(&self) -> Vec<Repo> {
        let mut seen: Vec<Repo> = Vec::new();
        for link in &self.prs {
            if !seen.contains(&link.repo) {
                seen.push(link.repo.clone());
            }
        }
        seen
    }
}
