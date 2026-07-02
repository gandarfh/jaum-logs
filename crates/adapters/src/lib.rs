//! jaum-logs edge adapters: shell-out to `git` and `gh`.
//! They keep the core tool-agnostic. Worktree and `gh` lack a good crate, so
//! they're driven via `std::process` (no git2 for worktree).

pub mod executor;
pub mod gh;
pub mod git;

pub use executor::{ClaudeExecutor, ExecFlags, Executor, Session};
pub use gh::Gh;
pub use git::Git;
