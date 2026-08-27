//! Reusable core of jaum-logs: the data model and the `.backlog/` store.
//! A thin, tool-agnostic shell — it writes no feature code, no RFC/ADR, and
//! performs no merges. Adapters (git, gh, executor, ui) live in separate crates.

pub mod error;
pub mod model;
pub mod store;

pub use error::JaumError;
pub use model::{Constraint, Enforce, MergeState, PrLink, Repo, Status, Task, TaskType};
pub use store::Store;
