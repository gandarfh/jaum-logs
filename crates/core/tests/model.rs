use jaum_core::{CiStatus, Constraint, Enforce, MergeState, PrCi};

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
        link("org/b", 1, None),
        link("org/a", 2, None),
        link("org/b", 3, None),
    ];
    assert_eq!(t.linked_repos(), vec!["org/b", "org/a"]);
}

fn link(repo: &str, pr: u64, reviewed_sha: Option<&str>) -> jaum_core::PrLink {
    jaum_core::PrLink {
        repo: repo.into(),
        pr,
        branch: "b".into(),
        reviewed_sha: reviewed_sha.map(String::from),
    }
}

fn ci(state: MergeState, checks: CiStatus, head_sha: &str) -> PrCi {
    PrCi {
        state,
        checks,
        head_sha: head_sha.into(),
    }
}

fn task_with_links(links: Vec<jaum_core::PrLink>) -> jaum_core::Task {
    let mut t = task_with_body("");
    t.prs = links;
    t
}

#[test]
fn review_trigger_fires_when_every_pr_is_green() {
    let t = task_with_links(vec![link("org/a", 1, None), link("org/b", 2, None)]);
    let observed = vec![
        (
            "org/a".to_string(),
            ci(MergeState::Open, CiStatus::Passing, "aaa"),
        ),
        (
            "org/b".to_string(),
            ci(MergeState::Open, CiStatus::Passing, "bbb"),
        ),
    ];
    assert_eq!(
        t.review_trigger(&observed),
        Some(vec![
            ("org/a".to_string(), "aaa".to_string()),
            ("org/b".to_string(), "bbb".to_string()),
        ])
    );
}

#[test]
fn review_trigger_requires_all_prs_green() {
    let t = task_with_links(vec![link("org/a", 1, None), link("org/b", 2, None)]);
    for not_green in [
        CiStatus::Pending,
        CiStatus::Failing,
        CiStatus::NoChecks,
        CiStatus::Unknown,
    ] {
        let observed = vec![
            (
                "org/a".to_string(),
                ci(MergeState::Open, CiStatus::Passing, "aaa"),
            ),
            ("org/b".to_string(), ci(MergeState::Open, not_green, "bbb")),
        ];
        assert_eq!(t.review_trigger(&observed), None, "{not_green:?}");
    }
}

#[test]
fn review_trigger_requires_open_prs_and_known_repos() {
    let t = task_with_links(vec![link("org/a", 1, None)]);
    for state in [
        MergeState::Merged,
        MergeState::Closed,
        MergeState::NotCreated,
        MergeState::Unknown,
    ] {
        let observed = vec![("org/a".to_string(), ci(state, CiStatus::Passing, "aaa"))];
        assert_eq!(t.review_trigger(&observed), None, "{state:?}");
    }
    // repo missing from the observations: no decision possible
    let other = vec![(
        "org/z".to_string(),
        ci(MergeState::Open, CiStatus::Passing, "z"),
    )];
    assert_eq!(t.review_trigger(&other), None);
    // empty head SHA (gh gave no commit): no trigger
    let no_sha = vec![(
        "org/a".to_string(),
        ci(MergeState::Open, CiStatus::Passing, ""),
    )];
    assert_eq!(t.review_trigger(&no_sha), None);
}

#[test]
fn review_trigger_skips_spikes_missing_prs_and_uncreated_prs() {
    let green = vec![(
        "org/a".to_string(),
        ci(MergeState::Open, CiStatus::Passing, "aaa"),
    )];

    let mut spike = task_with_links(vec![link("org/a", 1, None)]);
    spike.task_type = jaum_core::TaskType::Spike;
    assert_eq!(spike.review_trigger(&green), None);

    let no_prs = task_with_links(Vec::new());
    assert_eq!(no_prs.review_trigger(&green), None);

    let uncreated = task_with_links(vec![link("org/a", 0, None)]);
    assert_eq!(uncreated.review_trigger(&green), None);
}

#[test]
fn review_trigger_is_idempotent_per_commit_and_rearms_on_new_push() {
    let reviewed = task_with_links(vec![link("org/a", 1, Some("aaa"))]);
    let same = vec![(
        "org/a".to_string(),
        ci(MergeState::Open, CiStatus::Passing, "aaa"),
    )];
    assert_eq!(
        reviewed.review_trigger(&same),
        None,
        "same SHA must not re-fire"
    );

    let moved = vec![(
        "org/a".to_string(),
        ci(MergeState::Open, CiStatus::Passing, "ccc"),
    )];
    assert_eq!(
        reviewed.review_trigger(&moved),
        Some(vec![("org/a".to_string(), "ccc".to_string())])
    );

    // multi-PR: one head moving is enough, and markers cover every link
    let multi = task_with_links(vec![
        link("org/a", 1, Some("aaa")),
        link("org/b", 2, Some("bbb")),
    ]);
    let one_moved = vec![
        (
            "org/a".to_string(),
            ci(MergeState::Open, CiStatus::Passing, "aaa"),
        ),
        (
            "org/b".to_string(),
            ci(MergeState::Open, CiStatus::Passing, "new"),
        ),
    ];
    assert_eq!(
        multi.review_trigger(&one_moved),
        Some(vec![
            ("org/a".to_string(), "aaa".to_string()),
            ("org/b".to_string(), "new".to_string()),
        ])
    );
}
