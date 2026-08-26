use super::*;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn valid_minimal() -> Vec<String> {
    args(&[
        "--type",
        "impl",
        "--objective",
        "do the thing",
        "--criteria",
        "it works",
    ])
}

#[test]
fn parses_minimal_valid_call() {
    let parsed = parse(&valid_minimal()).unwrap();
    assert_eq!(parsed.task_type, TaskType::Impl);
    assert_eq!(parsed.objective, "do the thing");
    assert_eq!(parsed.criteria, vec!["it works".to_string()]);
    assert!(parsed.rfcs.is_empty());
    assert!(parsed.adrs.is_empty());
    assert_eq!(parsed.repo, None);
    assert_eq!(parsed.branch, None);
}

#[test]
fn collects_repeated_flags_in_order() {
    let a = args(&[
        "--type",
        "impl",
        "--objective",
        "do the thing",
        "--criteria",
        "first",
        "--criteria",
        "second",
        "--rfc",
        "rfc-a",
        "--rfc",
        "rfc-b",
        "--adr",
        "adr-a",
    ]);
    let parsed = parse(&a).unwrap();
    assert_eq!(
        parsed.criteria,
        vec!["first".to_string(), "second".to_string()]
    );
    assert_eq!(parsed.rfcs, vec!["rfc-a".to_string(), "rfc-b".to_string()]);
    assert_eq!(parsed.adrs, vec!["adr-a".to_string()]);
}

#[test]
fn rejects_unknown_flag() {
    let a = args(&["--type", "impl", "--bogus", "x"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "unknown flag '--bogus'");
}

#[test]
fn rejects_flag_missing_value() {
    let a = args(&["--type"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--type requires a value");
}

#[test]
fn rejects_missing_type() {
    let a = args(&["--objective", "x", "--criteria", "y"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "missing required --type");
}

#[test]
fn rejects_invalid_type() {
    let a = args(&["--type", "refactor", "--objective", "x", "--criteria", "y"]);
    let err = parse(&a).unwrap_err();
    assert!(err.contains("'refactor'"));
    assert!(err.contains("impl, spike"));
}

#[test]
fn rejects_missing_objective() {
    let a = args(&["--type", "impl", "--criteria", "y"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "missing required --objective");
}

#[test]
fn rejects_blank_objective() {
    let a = args(&["--type", "impl", "--objective", "   ", "--criteria", "y"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--objective must not be blank");
}

#[test]
fn rejects_missing_criteria() {
    let a = args(&["--type", "impl", "--objective", "x"]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "at least one --criteria is required");
}

#[test]
fn rejects_blank_criteria() {
    let a = args(&["--type", "impl", "--objective", "x", "--criteria", "  "]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--criteria must not be blank");
}

#[test]
fn rejects_repo_without_branch() {
    let mut a = valid_minimal();
    a.extend(args(&["--repo", "org/app"]));
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--repo requires --branch (a PR link needs both)");
}

#[test]
fn accepts_branch_without_repo() {
    let mut a = valid_minimal();
    a.extend(args(&["--branch", "feat/x"]));
    let parsed = parse(&a).unwrap();
    assert_eq!(parsed.branch, Some("feat/x".to_string()));
    assert_eq!(parsed.repo, None);
}

#[test]
fn rejects_repo_and_branch_on_spike() {
    let a = args(&[
        "--type",
        "spike",
        "--objective",
        "x",
        "--criteria",
        "y",
        "--repo",
        "org/app",
        "--branch",
        "feat/x",
    ]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--repo/--branch cannot be used with --type spike");
}

#[test]
fn rejects_branch_only_on_spike() {
    let a = args(&[
        "--type",
        "spike",
        "--objective",
        "x",
        "--criteria",
        "y",
        "--branch",
        "feat/x",
    ]);
    let err = parse(&a).unwrap_err();
    assert_eq!(err, "--repo/--branch cannot be used with --type spike");
}

#[test]
fn render_body_matches_new_task_quick_shape() {
    let body = render_body("do the thing", &["first".to_string(), "second".to_string()]);
    assert_eq!(
        body,
        "## Objective\n\ndo the thing\n\n## Acceptance criteria\n- [ ] first\n- [ ] second\n"
    );
}

#[test]
fn render_body_with_no_criteria() {
    let body = render_body("do the thing", &[]);
    assert_eq!(
        body,
        "## Objective\n\ndo the thing\n\n## Acceptance criteria\n"
    );
}
