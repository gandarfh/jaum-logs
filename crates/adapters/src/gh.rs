use std::process::Command;

use anyhow::{Context, Result, bail};
use jaum_core::MergeState;

/// Adapter de `gh` via shell-out. O GitHub é downstream: a ferramenta cria PR
/// e **lê** número e estado de merge, nunca mergeia.
pub struct Gh {
    bin: String,
}

impl Default for Gh {
    fn default() -> Self {
        Self::new()
    }
}

impl Gh {
    pub fn new() -> Self {
        Self { bin: "gh".to_string() }
    }

    /// Permite apontar para um binário alternativo (usado em testes).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Abre um PR para `branch` no `repo` (slug "owner/name"). Devolve o número.
    /// NUNCA mergeia — só cria.
    pub fn pr_create(&self, repo: &str, branch: &str) -> Result<u64> {
        let out = self.run(&[
            "pr", "create", "--repo", repo, "--head", branch, "--fill",
        ])?;
        parse_pr_number_from_url(&out)
            .with_context(|| format!("extraindo número do PR da saída do gh: {out:?}"))
    }

    /// Número do PR aberto/fechado para `branch`, ou `0` se nenhum existe.
    pub fn pr_number(&self, repo: &str, branch: &str) -> Result<u64> {
        let out = self.run(&[
            "pr", "list", "--repo", repo, "--head", branch, "--state", "all", "--json", "number",
            "--jq", ".[0].number // 0",
        ])?;
        let s = out.trim();
        if s.is_empty() {
            return Ok(0);
        }
        s.parse()
            .with_context(|| format!("parseando número do PR: {s:?}"))
    }

    /// Estado de merge do PR. `pr == 0` é tratado como `NotCreated` sem chamar o gh.
    pub fn pr_merge_state(&self, repo: &str, pr: u64) -> Result<MergeState> {
        if pr == 0 {
            return Ok(MergeState::NotCreated);
        }
        let out = self.run(&[
            "pr", "view", &pr.to_string(), "--repo", repo, "--json", "state", "--jq", ".state",
        ])?;
        Ok(match out.trim() {
            "MERGED" => MergeState::Merged,
            "OPEN" => MergeState::Open,
            "CLOSED" => MergeState::Closed,
            _ => MergeState::Unknown,
        })
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("executando {} {args:?}", self.bin))?;
        if !out.status.success() {
            bail!(
                "{} {args:?} falhou: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Extrai o número do PR da URL emitida pelo `gh pr create`. O gh pode imprimir
/// avisos antes da URL, então usamos a última linha não vazia.
fn parse_pr_number_from_url(s: &str) -> Option<u64> {
    let line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    line.trim().rsplit('/').next()?.parse().ok()
}
