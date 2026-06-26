use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Estado da task no fluxo. É a única fonte de verdade do progresso —
/// merge de PR é lido do `gh`, nunca espelhado aqui (apenas `merged` final).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Backlog,
    Ready,
    Wip,
    Review,
    Merged,
}

/// Natureza da task. `Spike` gera documento (RFC/ADR), não tem PR nem play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskType {
    Impl,
    Spike,
}

/// Classifica como uma constraint é garantida (os dois patches do core):
/// `Hook` = mecânica, bloqueada preventivamente por PreToolUse hook;
/// `Review` = semântica, checada item a item e obrigatória no review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforce {
    Hook,
    Review,
}

/// Vínculo task↔PR. `pr == 0` significa "ainda não criado": o número real é
/// lido do `gh` (downstream), nunca inventado à mão.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    pub repo: String,
    #[serde(default)]
    pub pr: u64,
    pub branch: String,
}

/// Diretriz "não faça X" anexada à task, classificada por como é garantida.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub text: String,
    pub enforce: Enforce,
    /// Regex que o PreToolUse hook usa para bloquear (`enforce: hook`). Quando
    /// ausente, [`Constraint::hook_pattern`] deriva uma heurística do `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

impl Constraint {
    /// Padrão (regex estendida, usada com `grep -iE` no hook) para bloquear esta
    /// constraint. Usa `pattern` explícito se houver; senão, deriva do `text`:
    /// prioriza um token que pareça caminho (`src/legacy/`), e na falta usa as
    /// palavras significativas (descartando stopwords PT) numa alternância.
    pub fn hook_pattern(&self) -> String {
        if let Some(p) = &self.pattern {
            return p.clone();
        }
        let lower = self.text.to_lowercase();
        // 1) token que parece caminho/arquivo (contém `/` ou `.`)
        if let Some(path) = lower
            .split_whitespace()
            .find(|t| t.contains('/') || t.contains('.'))
        {
            return escape_regex(path.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.'));
        }
        // 2) palavras significativas
        const STOP: &[&str] = &[
            "nao", "não", "em", "de", "da", "do", "a", "o", "os", "as", "no", "na",
            "sem", "com", "e", "ou", "manter", "rodar", "tocar", "usar", "fazer",
            "novo", "nova", "para", "pra", "que", "se",
        ];
        let words: Vec<String> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2 && !STOP.contains(w))
            .map(escape_regex)
            .collect();
        if words.is_empty() {
            // fallback: o texto inteiro escapado (nunca casa vazio)
            return escape_regex(lower.trim());
        }
        words.join("|")
    }
}

/// Escapa metacaracteres de regex estendida (ERE) para casar literalmente.
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

/// Slug "owner/name" de um repositório. Alias por enquanto; vira newtype se a
/// fase dos adapters (git/gh) precisar de mais semântica.
pub type Repo = String;

/// Estado de merge de um PR, **lido** do `gh` — nunca espelhado no markdown.
/// Base do `finish`, que atualiza o status da task a partir daqui sem nunca
/// executar o merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    /// `pr == 0`: ainda não criado.
    NotCreated,
    /// PR aberto, ainda não mergeado.
    Open,
    /// Mergeado.
    Merged,
    /// Fechado sem merge.
    Closed,
    /// Estado não reconhecido devolvido pelo `gh`.
    Unknown,
}

/// Uma task do `.backlog/`. Os campos sem `skip` são exatamente o frontmatter
/// YAML; `body` e `path` são preenchidos pelo store após o parse e não são
/// serializados de volta.
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

    /// Constraints de uma classe de enforcement (Hook = mecânica, Review = semântica).
    pub fn constraints_by(&self, kind: Enforce) -> Vec<&Constraint> {
        self.constraints.iter().filter(|c| c.enforce == kind).collect()
    }

    /// Repos linkados via PRs, deduplicados preservando a ordem.
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
