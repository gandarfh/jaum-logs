use thiserror::Error;

/// Erros de domínio do core. O store devolve `anyhow::Result`, mas embrulha
/// estas variantes para que chamadores possam fazer downcast e reagir a casos
/// específicos (ex.: task inexistente) sem casar string de mensagem.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum JaumError {
    #[error("task `{0}` não encontrada")]
    TaskNotFound(String),

    #[error("frontmatter ausente ou inválido em {path}")]
    MalformedFrontmatter { path: String },

    #[error("PR para o repo `{repo}` não está vinculado à task `{id}`")]
    PrLinkNotFound { id: String, repo: String },
}
