//! Parallelism: flags (never blocks) overlap between tasks in the same repo and
//! manages per-resource locks (port/build/db) stored in the frontmatter.

use anyhow::{Result, bail};
use jaum_core::{Status, Store};

/// View of an "active session" — derived from state (status `wip`), not from PIDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub repos: Vec<String>,
    pub branches: Vec<String>,
    pub locks: Vec<String>,
}

pub struct Conflict<'a> {
    store: &'a Store,
}

impl<'a> Conflict<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    /// Pairs of `wip` tasks that share a repo. Flags concurrency (alert only,
    /// never blocks). Deterministic (ordered by id).
    pub fn detect_overlap(&self) -> Result<Vec<(String, String, String)>> {
        let wip = self.store.list(Some(Status::Wip))?;
        let mut out = Vec::new();
        for i in 0..wip.len() {
            for j in (i + 1)..wip.len() {
                let a = &wip[i];
                let b = &wip[j];
                let rb = b.linked_repos();
                for repo in a.linked_repos() {
                    if rb.contains(&repo) {
                        out.push((a.id.clone(), b.id.clone(), repo));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Tasks in `wip` (proxy for an active session).
    pub fn active_sessions(&self) -> Result<Vec<SessionInfo>> {
        Ok(self
            .store
            .list(Some(Status::Wip))?
            .into_iter()
            .map(|t| SessionInfo {
                id: t.id.clone(),
                repos: t.linked_repos(),
                branches: t.prs.iter().map(|p| p.branch.clone()).collect(),
                locks: t.locks.clone(),
            })
            .collect())
    }

    /// Currently held locks: `(resource, holding task)`.
    pub fn held_locks(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for t in self.store.list(None)? {
            for lock in &t.locks {
                out.push((lock.clone(), t.id.clone()));
            }
        }
        Ok(out)
    }

    /// Acquires a resource lock for the task. Fails if another task holds it.
    pub fn lock_acquire(&self, id: &str, resource: &str) -> Result<()> {
        for t in self.store.list(None)? {
            if t.id != id && t.locks.iter().any(|l| l == resource) {
                bail!("resource `{resource}` is already locked by {}", t.id);
            }
        }
        let mut task = self.store.get(id)?;
        if !task.locks.iter().any(|l| l == resource) {
            task.locks.push(resource.to_string());
            self.store.write(&task)?;
        }
        Ok(())
    }

    /// Releases a resource lock held by the task.
    pub fn lock_release(&self, id: &str, resource: &str) -> Result<()> {
        let mut task = self.store.get(id)?;
        task.locks.retain(|l| l != resource);
        self.store.write(&task)?;
        Ok(())
    }
}
