use thiserror::Error;

/// Core domain errors. The store returns `anyhow::Result` but wraps these
/// variants so callers can downcast and react to specific cases (e.g. a missing
/// task) without matching on message strings.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum JaumError {
    #[error("task `{0}` not found")]
    TaskNotFound(String),

    #[error("missing or invalid frontmatter in {path}")]
    MalformedFrontmatter { path: String },

    #[error("PR for repo `{repo}` is not linked to task `{id}`")]
    PrLinkNotFound { id: String, repo: String },
}
