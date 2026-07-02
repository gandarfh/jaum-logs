use jaum_core::{Constraint, Enforce};

fn c(text: &str, pattern: Option<&str>) -> Constraint {
    Constraint {
        text: text.to_string(),
        enforce: Enforce::Hook,
        pattern: pattern.map(String::from),
    }
}

#[test]
fn hook_pattern_uses_explicit_pattern_when_present() {
    let c = c("do not touch src/legacy/", Some("src/(legacy|old)/"));
    assert_eq!(c.hook_pattern(), "src/(legacy|old)/");
}

#[test]
fn hook_pattern_derives_path_from_text() {
    let c = c("do not touch src/legacy/", None);
    // path extracted, with `.` escaped if it had one
    assert_eq!(c.hook_pattern(), "src/legacy/");
}

#[test]
fn hook_pattern_escapes_dot_in_path() {
    let c = c("do not edit config.toml", None);
    assert_eq!(c.hook_pattern(), r"config\.toml");
}

#[test]
fn hook_pattern_derives_keyword_when_no_path() {
    let c = c("do not run migration", None);
    // stopwords (not, run) dropped
    assert_eq!(c.hook_pattern(), "migration");
}

#[test]
fn hook_pattern_alternates_multiple_significant_words() {
    let c = c("do not use reflection runtime", None);
    let p = c.hook_pattern();
    assert!(p.contains("reflection"));
    assert!(p.contains("runtime"));
    assert!(p.contains('|'));
}

#[test]
fn hook_pattern_falls_back_to_whole_text_when_only_stopwords() {
    // every word is a stopword or too short, so the escaped text is the pattern
    let c = c("not any all", None);
    assert_eq!(c.hook_pattern(), "not any all");
}

#[test]
fn hook_pattern_escapes_regex_metacharacters_in_fallback() {
    let c = c("no (a) $x", None);
    assert_eq!(c.hook_pattern(), r"no \(a\) \$x");
}

fn task_with_body(body: &str) -> jaum_core::Task {
    jaum_core::Task {
        id: "TASK-001".into(),
        task_type: jaum_core::TaskType::Impl,
        status: jaum_core::Status::Backlog,
        rfcs: Vec::new(),
        adrs: Vec::new(),
        prs: Vec::new(),
        deferred: Vec::new(),
        constraints: Vec::new(),
        locks: Vec::new(),
        body: body.into(),
        path: None,
    }
}

#[test]
fn acceptance_criteria_reads_checkboxes_and_plain_items() {
    let t = task_with_body(
        "## Objective\n- not a criterion\n\n## Acceptance criteria\n- [ ] first\n- [x] second\n* third\n-\n\n## Notes\n- also not one\n",
    );
    assert_eq!(t.acceptance_criteria(), vec!["first", "second", "third"]);
}

#[test]
fn acceptance_criteria_reopens_after_other_section() {
    let t = task_with_body(
        "## Acceptance criteria\n- one\n## Notes\n- skip\n### More acceptance items\n- two\nplain line\n",
    );
    assert_eq!(t.acceptance_criteria(), vec!["one", "two"]);
}

#[test]
fn acceptance_criteria_empty_without_section() {
    let t = task_with_body("## Objective\n- something\n");
    assert!(t.acceptance_criteria().is_empty());
}

#[test]
fn is_spike_and_constraints_by_kind() {
    let mut t = task_with_body("");
    assert!(!t.is_spike());
    t.task_type = jaum_core::TaskType::Spike;
    assert!(t.is_spike());

    t.constraints = vec![
        Constraint {
            text: "mechanical".into(),
            enforce: Enforce::Hook,
            pattern: None,
        },
        Constraint {
            text: "semantic".into(),
            enforce: Enforce::Review,
            pattern: None,
        },
    ];
    let hooks = t.constraints_by(Enforce::Hook);
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].text, "mechanical");
    let reviews = t.constraints_by(Enforce::Review);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].text, "semantic");
}

#[test]
fn linked_repos_dedups_preserving_order() {
    let mut t = task_with_body("");
    t.prs = vec![
        jaum_core::PrLink {
            repo: "org/b".into(),
            pr: 1,
            branch: "x".into(),
        },
        jaum_core::PrLink {
            repo: "org/a".into(),
            pr: 2,
            branch: "y".into(),
        },
        jaum_core::PrLink {
            repo: "org/b".into(),
            pr: 3,
            branch: "z".into(),
        },
    ];
    assert_eq!(t.linked_repos(), vec!["org/b", "org/a"]);
}
