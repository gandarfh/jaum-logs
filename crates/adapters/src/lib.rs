//! Adapters de borda do jaum-logs: shell-out para `git` e `gh`.
//! Mantêm o core tool-agnóstico. Worktree e `gh` não têm crate boa, então
//! são acionados via `std::process` (sem git2 para worktree).

pub mod gh;
pub mod git;

pub use gh::Gh;
pub use git::Git;
