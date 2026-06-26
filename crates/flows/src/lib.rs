//! Flows de orquestração do jaum-logs: combinam store + adapters para executar
//! o fluxo (play, e nas próximas fases review/ingest/docs/conflict/finish).
//! Casca fina: orquestra, não escreve código de feature nem faz merge.

pub mod play;
