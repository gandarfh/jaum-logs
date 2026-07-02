//! Finish: lê o estado de merge dos PRs via `gh` e atualiza o status da task.
//! NUNCA executa merge — o merge é comando do usuário, fora desta ferramenta.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use jaum_adapters::Gh;
use jaum_core::{MergeState, PrLink, Status, Store};

pub struct Finish<'a> {
    store: &'a Store,
    gh: &'a Gh,
    /// slug "owner/name" -> caminho local do repo (o `gh` roda lá dentro).
    repos: HashMap<String, PathBuf>,
}

impl<'a> Finish<'a> {
    pub fn new(store: &'a Store, gh: &'a Gh, repos: HashMap<String, PathBuf>) -> Self {
        Self { store, gh, repos }
    }

    /// Estado de merge efetivo de um PR. O `gh` roda no diretório do repo (resolvido
    /// pelo slug). Tolerante: sem repo mapeado, ou se o `gh` falhar (sem remote no
    /// GitHub, sem autenticação), trata como `NotCreated`/`Unknown` em vez de
    /// propagar o erro.
    fn pr_state(&self, link: &PrLink) -> Result<(u64, MergeState)> {
        let Some(dir) = self.repos.get(&link.repo) else {
            // repo não mapeado: nada a consultar.
            return Ok((link.pr, MergeState::NotCreated));
        };
        let pr = if link.pr != 0 {
            link.pr
        } else {
            // descoberta best-effort: falha do gh = ainda não há PR.
            self.gh.pr_number(dir, &link.branch).unwrap_or(0)
        };
        if pr == 0 {
            return Ok((0, MergeState::NotCreated));
        }
        let state = self.gh.pr_merge_state(dir, pr)?;
        Ok((pr, state))
    }

    /// Estado de merge agregado da task (somente leitura; não muda status).
    pub fn merge_state(&self, id: &str) -> Result<MergeState> {
        let task = self.store.get(id)?;
        let mut states = Vec::new();
        for link in &task.prs {
            states.push(self.pr_state(link)?.1);
        }
        Ok(aggregate(&states))
    }

    /// Lê o estado de merge, persiste números de PR recém-descobertos e, se TUDO
    /// está merged, marca a task como `merged`. Não mergeia nada.
    pub fn run(&self, id: &str) -> Result<MergeState> {
        let task = self.store.get(id)?;
        let mut states = Vec::new();
        for link in &task.prs {
            let (pr, state) = self.pr_state(link)?;
            // persiste o número descoberto (downstream: número é lido, não inventado)
            if link.pr == 0 && pr != 0 {
                self.store.set_pr(id, &link.repo, pr)?;
            }
            states.push(state);
        }
        let agg = aggregate(&states);
        if agg == MergeState::Merged {
            self.store.set_status(id, Status::Merged)?;
        }
        Ok(agg)
    }
}

/// Agrega os estados dos PRs num único veredito da task.
fn aggregate(states: &[MergeState]) -> MergeState {
    if states.is_empty() {
        return MergeState::NotCreated;
    }
    if states.iter().all(|s| *s == MergeState::Merged) {
        return MergeState::Merged;
    }
    if states.contains(&MergeState::Open) {
        return MergeState::Open;
    }
    if states.contains(&MergeState::NotCreated) {
        return MergeState::NotCreated;
    }
    if states.contains(&MergeState::Unknown) {
        return MergeState::Unknown;
    }
    MergeState::Closed
}
