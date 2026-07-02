//! Flows de orquestração do jaum-logs: combinam store + adapters para executar
//! o fluxo (play, e nas próximas fases review/ingest/docs/conflict/finish).
//! Casca fina: orquestra, não escreve código de feature nem faz merge.

/// Modelo do claude usado nas sessões interativas de play/review/setup.
/// Centralizado aqui para trocar num único lugar.
pub const AGENT_MODEL: &str = "claude-fable-5";

pub mod conflict;
pub mod finish;
pub mod ingest;
pub mod parallel;
pub mod play;
pub mod review;
pub mod setup;
