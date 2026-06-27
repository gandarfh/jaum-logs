//! Paralelo: sinaliza (não bloqueia) overlap entre tasks no mesmo repo e
//! gerencia locks por recurso (porta/build/db) gravados no frontmatter.

use anyhow::{Result, bail};
use jaum_core::{Status, Store};

/// Visão de uma "sessão ativa" — derivada do estado (status `wip`), não de PIDs.
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

    /// Pares de tasks em `wip` que compartilham um repo. Sinaliza concorrência
    /// (apenas alerta — não impede). Determinístico (ordenado por id).
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

    /// Tasks em `wip` (proxy de sessão ativa).
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

    /// Locks atualmente detidos: `(recurso, task que detém)`.
    pub fn held_locks(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for t in self.store.list(None)? {
            for lock in &t.locks {
                out.push((lock.clone(), t.id.clone()));
            }
        }
        Ok(out)
    }

    /// Adquire um lock de recurso para a task. Falha se outra task já o detém.
    pub fn lock_acquire(&self, id: &str, resource: &str) -> Result<()> {
        for t in self.store.list(None)? {
            if t.id != id && t.locks.iter().any(|l| l == resource) {
                bail!("recurso `{resource}` já está travado por {}", t.id);
            }
        }
        let mut task = self.store.get(id)?;
        if !task.locks.iter().any(|l| l == resource) {
            task.locks.push(resource.to_string());
            self.store.write(&task)?;
        }
        Ok(())
    }

    /// Libera um lock de recurso detido pela task.
    pub fn lock_release(&self, id: &str, resource: &str) -> Result<()> {
        let mut task = self.store.get(id)?;
        task.locks.retain(|l| l != resource);
        self.store.write(&task)?;
        Ok(())
    }
}
