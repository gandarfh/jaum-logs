//! Core reaproveitável do jaum-logs: modelo de dados e store do `.backlog/`.
//! Casca fina e tool-agnóstica — não escreve código de feature, RFC/ADR, nem
//! faz merge. Adapters (git, gh, executor, ui) vivem em crates separados.

pub mod error;
pub mod model;
pub mod store;

pub use error::JaumError;
pub use model::{Constraint, Enforce, MergeState, PrLink, Repo, Status, Task, TaskType};
pub use store::Store;
