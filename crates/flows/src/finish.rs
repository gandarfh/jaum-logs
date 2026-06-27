//! Finish: lê o estado de merge dos PRs via `gh` e atualiza o status da task.
//! NUNCA executa merge — o merge é comando do usuário, fora desta ferramenta.

use anyhow::Result;
use jaum_adapters::Gh;
use jaum_core::{MergeState, PrLink, Status, Store};

pub struct Finish<'a> {
    store: &'a Store,
    gh: &'a Gh,
}

impl<'a> Finish<'a> {
    pub fn new(store: &'a Store, gh: &'a Gh) -> Self {
        Self { store, gh }
    }

    /// Estado de merge efetivo de um PR. Descobre o número via `gh` se ainda for
    /// 0 (não persiste aqui — só leitura).
    fn pr_state(&self, link: &PrLink) -> Result<(u64, MergeState)> {
        let pr = if link.pr != 0 {
            link.pr
        } else {
            self.gh.pr_number(&link.repo, &link.branch)?
        };
        let state = self.gh.pr_merge_state(&link.repo, pr)?;
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
