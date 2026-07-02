//! Finish: reads the merge state of PRs via `gh` and updates the task status.
//! NEVER merges — merging is the user's command, outside this tool.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use jaum_adapters::Gh;
use jaum_core::{MergeState, PrLink, Status, Store};

pub struct Finish<'a> {
    store: &'a Store,
    gh: &'a Gh,
    /// slug "owner/name" -> local repo path (`gh` runs inside it).
    repos: HashMap<String, PathBuf>,
}

impl<'a> Finish<'a> {
    pub fn new(store: &'a Store, gh: &'a Gh, repos: HashMap<String, PathBuf>) -> Self {
        Self { store, gh, repos }
    }

    /// Effective merge state of a PR. `gh` runs in the repo directory (resolved
    /// by slug). Tolerant: with no mapped repo, or if `gh` fails (no GitHub
    /// remote, no auth), treats it as `NotCreated`/`Unknown` instead of
    /// propagating the error.
    fn pr_state(&self, link: &PrLink) -> Result<(u64, MergeState)> {
        let Some(dir) = self.repos.get(&link.repo) else {
            // repo not mapped: nothing to query.
            return Ok((link.pr, MergeState::NotCreated));
        };
        let pr = if link.pr != 0 {
            link.pr
        } else {
            // best-effort discovery: gh failure = no PR yet.
            self.gh.pr_number(dir, &link.branch).unwrap_or(0)
        };
        if pr == 0 {
            return Ok((0, MergeState::NotCreated));
        }
        let state = self.gh.pr_merge_state(dir, pr)?;
        Ok((pr, state))
    }

    /// Aggregated merge state of the task (read-only; does not change status).
    pub fn merge_state(&self, id: &str) -> Result<MergeState> {
        let task = self.store.get(id)?;
        let mut states = Vec::new();
        for link in &task.prs {
            states.push(self.pr_state(link)?.1);
        }
        Ok(aggregate(&states))
    }

    /// Reads the merge state, persists newly discovered PR numbers, and if ALL
    /// are merged, marks the task as `merged`. Merges nothing.
    pub fn run(&self, id: &str) -> Result<MergeState> {
        let task = self.store.get(id)?;
        let mut states = Vec::new();
        for link in &task.prs {
            let (pr, state) = self.pr_state(link)?;
            // persist the discovered number (downstream reads it, never invents it)
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

/// Aggregates the PR states into a single verdict for the task.
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
