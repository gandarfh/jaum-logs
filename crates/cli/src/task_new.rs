//! Pure parsing and validation for `jaum task new` — argv tokens in, a
//! validated [`NewTaskArgs`] or a stable error message out. No filesystem or
//! config access, so a caller (human, script, or a Claude Code Skill) gets a
//! deterministic error before anything is written to `.backlog/`.

use jaum_core::TaskType;

const KNOWN_FLAGS: [&str; 7] = [
    "--type",
    "--objective",
    "--criteria",
    "--rfc",
    "--adr",
    "--repo",
    "--branch",
];

#[derive(Debug, PartialEq, Eq)]
pub struct NewTaskArgs {
    pub task_type: TaskType,
    pub objective: String,
    pub criteria: Vec<String>,
    pub rfcs: Vec<String>,
    pub adrs: Vec<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
}

/// Parses the tokens after `task new`. Unknown flags / missing values are
/// reported as soon as encountered (left to right); once tokenizing
/// succeeds, semantic checks run in a fixed order: type, objective,
/// criteria, repo/branch coherence.
pub fn parse(args: &[String]) -> Result<NewTaskArgs, String> {
    let mut task_type: Option<String> = None;
    let mut objective: Option<String> = None;
    let mut criteria: Vec<String> = Vec::new();
    let mut rfcs: Vec<String> = Vec::new();
    let mut adrs: Vec<String> = Vec::new();
    let mut repo: Option<String> = None;
    let mut branch: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if !KNOWN_FLAGS.contains(&flag) {
            return Err(format!("unknown flag '{flag}'"));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?
            .clone();
        match flag {
            "--type" => task_type = Some(value),
            "--objective" => objective = Some(value),
            "--criteria" => criteria.push(value),
            "--rfc" => rfcs.push(value),
            "--adr" => adrs.push(value),
            "--repo" => repo = Some(value),
            "--branch" => branch = Some(value),
            _ => unreachable!("filtered by KNOWN_FLAGS above"),
        }
        i += 2;
    }

    let task_type = task_type
        .ok_or_else(|| "missing required --type".to_string())?
        .parse::<TaskType>()?;

    let objective = objective.ok_or_else(|| "missing required --objective".to_string())?;
    if objective.trim().is_empty() {
        return Err("--objective must not be blank".to_string());
    }

    if criteria.is_empty() {
        return Err("at least one --criteria is required".to_string());
    }
    if criteria.iter().any(|c| c.trim().is_empty()) {
        return Err("--criteria must not be blank".to_string());
    }

    if repo.is_some() && branch.is_none() {
        return Err("--repo requires --branch (a PR link needs both)".to_string());
    }

    if task_type == TaskType::Spike && (repo.is_some() || branch.is_some()) {
        return Err("--repo/--branch cannot be used with --type spike".to_string());
    }

    Ok(NewTaskArgs {
        task_type,
        objective,
        criteria,
        rfcs,
        adrs,
        repo,
        branch,
    })
}

/// Renders the task body from the objective + criteria, in the same
/// `## Objective` / `## Acceptance criteria` shape as `App::new_task_quick`.
pub fn render_body(objective: &str, criteria: &[String]) -> String {
    let mut body = format!("## Objective\n\n{objective}\n\n## Acceptance criteria\n");
    for item in criteria {
        body.push_str(&format!("- [ ] {item}\n"));
    }
    body
}

#[cfg(test)]
#[path = "task_new_tests.rs"]
mod task_new_tests;
