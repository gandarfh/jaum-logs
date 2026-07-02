use std::cell::RefCell;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Gh, Git, Session};
use jaum_core::Store;
use jaum_flows::review::{
    ConstraintResult, ConstraintVerdict, Finding, Review, ReviewReport, Severity, read_only_flags,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-review-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const FIXTURE: &str = r#"---
id: TASK-001
type: impl
status: review
rfcs: [RFC-003]
prs:
  - repo: myorg/repo
    pr: 7
    branch: feat/task-001
constraints:
  - text: "do not touch src/legacy/"
    enforce: hook
  - text: "keep the API stable"
    enforce: review
  - text: "sem abstracao nova"
    enforce: review
---

## Objective
x
"#;

/// Fake executor: spawn_interactive returns a real session over `cat`.
struct FakeExec;
impl Executor for FakeExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

/// Executor that returns a fixed `stream-json` output (the default
/// `spawn_oneshot_streaming` feeds the lines from `spawn_oneshot`).
struct StreamExec(String);
impl Executor for StreamExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

fn setup(
    dir: &TmpDir,
) -> (
    Store,
    Git,
    Gh,
    PathBuf,
    std::collections::HashMap<String, PathBuf>,
) {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("RFC-003.md"), "# RFC-003\nenum aberto\n").unwrap();
    fs::write(docs.join("ADR-011.md"), "# ADR-011\ndecisao\n").unwrap();
    let repos_root = dir.0.join("repos");
    git_init(&repos_root.join("repo"));
    let repos =
        std::collections::HashMap::from([("myorg/repo".to_string(), repos_root.join("repo"))]);
    // fake gh: `true` ignores args and returns empty (no network in tests).
    (
        Store::new(&backlog),
        Git::new(),
        Gh::with_bin("true"),
        docs,
        repos,
    )
}

fn git_init(repo: &Path) {
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    fs::create_dir_all(repo).unwrap();
    run(&["init", "-b", "main", "-q"]);
    run(&["config", "user.email", "t@test.dev"]);
    run(&["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "x\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-qm", "init"]);
    run(&["branch", "feat/task-001"]);
}

// --- read-only ------------------------------------------------------------

#[test]
fn read_only_flags_block_all_writes() {
    let f = read_only_flags();
    for t in ["Edit", "Write", "NotebookEdit", "Bash"] {
        assert!(
            f.disallowed_tools.iter().any(|x| x == t),
            "missing block for {t}"
        );
    }
    assert_eq!(f.model.as_deref(), Some(jaum_flows::AGENT_MODEL));
}

// --- is_clean (pure) ------------------------------------------------------

fn report(findings: Vec<Finding>, constraints: Vec<ConstraintResult>) -> ReviewReport {
    ReviewReport {
        task: "TASK-001".into(),
        findings,
        constraints,
        criteria: Vec::new(),
    }
}
fn cr(text: &str, v: ConstraintVerdict) -> ConstraintResult {
    ConstraintResult {
        text: text.into(),
        verdict: v,
        note: String::new(),
    }
}

#[test]
fn is_clean_only_with_zero_findings_and_all_ok() {
    let clean = report(
        vec![],
        vec![cr("keep the API stable", ConstraintVerdict::Ok)],
    );
    assert!(clean.is_clean());
}

#[test]
fn is_clean_fails_with_finding() {
    let r = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "signature changed".into(),
            reference: Some("RFC-003".into()),
            severity: Severity::Major,
        }],
        vec![cr("keep the API stable", ConstraintVerdict::Ok)],
    );
    assert!(!r.is_clean());
}

#[test]
fn is_clean_fails_with_rejected_or_pending_constraint() {
    let rejected = report(vec![], vec![cr("x", ConstraintVerdict::Failed)]);
    assert!(!rejected.is_clean());
    let pending = report(vec![], vec![cr("x", ConstraintVerdict::Pending)]);
    assert!(!pending.is_clean(), "pending must not pass");
}

fn finding(message: &str, severity: Severity) -> Finding {
    Finding {
        file: "x.rs".into(),
        line: None,
        message: message.into(),
        reference: None,
        severity,
    }
}

#[test]
fn minor_and_nit_do_not_fail_only_blocker_and_major() {
    // minor/nit: informational, don't fail.
    let only_minor = report(vec![finding("cosmetic", Severity::Minor)], vec![]);
    assert!(only_minor.is_clean(), "minor must not fail");
    assert_eq!(only_minor.blocking_count(), 0);

    // major: fails.
    let major = report(vec![finding("bug importante", Severity::Major)], vec![]);
    assert!(!major.is_clean());
    assert_eq!(major.blocking_count(), 1);

    // blocker too.
    let blk = report(vec![finding("quebra", Severity::Blocker)], vec![]);
    assert!(!blk.is_clean());
    assert_eq!(blk.blocking_count(), 1);
}

// --- context and checklist ------------------------------------------------

#[test]
fn check_semantic_constraints_only_takes_enforce_review_as_pending() {
    let dir = TmpDir::new("checklist");
    let (store, git, gh, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let items = review.check_semantic_constraints("TASK-001").unwrap();
    assert_eq!(items.len(), 2); // only the enforce: review ones
    assert!(
        items
            .iter()
            .all(|i| i.verdict == ConstraintVerdict::Pending)
    );
    assert!(items.iter().any(|i| i.text == "keep the API stable"));
    // the enforce: hook does NOT enter
    assert!(!items.iter().any(|i| i.text.contains("src/legacy")));
}

#[test]
fn build_context_brings_docs_diff_and_checklist() {
    let dir = TmpDir::new("context");
    let (store, git, gh, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let ctx = review.build_context("TASK-001").unwrap();
    assert!(ctx.contains("RFC-003")); // all docs
    assert!(ctx.contains("ADR-011"));
    assert!(ctx.contains("PR diffs"));
    assert!(ctx.contains("myorg/repo"));
    assert!(ctx.contains("keep the API stable")); // checklist
    assert!(ctx.contains("sem abstracao nova"));
    // the working tree is pointed out for reading (Read/Grep/Glob)
    assert!(ctx.contains("Working tree"));
}

/// Fake `gh` that returns diff + PR title/body.
fn fake_gh(dir: &TmpDir) -> Gh {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.0.join("gh");
    fs::write(
        &path,
        "#!/usr/bin/env bash\n\
if [ \"$1\" = pr ] && [ \"$2\" = diff ]; then\n  printf 'diff --git a/x b/x\\n+novo\\n'\n\
elif [ \"$1\" = pr ] && [ \"$2\" = view ]; then\n  printf '{\"title\":\"Add parser\",\"body\":\"Implements the deck parser\"}\\n'\n\
fi\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    Gh::with_bin(path.to_string_lossy().into_owned())
}

#[test]
fn build_context_pulls_diff_and_pr_description_via_gh() {
    let dir = TmpDir::new("ghpr");
    let (store, git, _gh, docs, repos) = setup(&dir);
    let gh = fake_gh(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let ctx = review.build_context("TASK-001").unwrap();
    assert!(ctx.contains("PR #7: Add parser")); // PR title
    assert!(ctx.contains("Implements the deck parser")); // PR body
    assert!(ctx.contains("+novo")); // diff coming from gh
}

// --- persistence ----------------------------------------------------------

#[test]
fn write_and_load_report_roundtrip() {
    let dir = TmpDir::new("persist");
    let (store, git, gh, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let original = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "signature changed".into(),
            reference: Some("RFC-003".into()),
            severity: Severity::Major,
        }],
        vec![
            cr("keep the API stable", ConstraintVerdict::Failed),
            cr("sem abstracao nova", ConstraintVerdict::Ok),
        ],
    );
    review.write_report(&original).unwrap();

    // file in the right place and readable
    let path = dir.0.join(".backlog/TASK-001.review.md");
    assert!(path.exists());
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("src/api.rs:42 - signature changed (violates RFC-003)"));
    assert!(raw.contains("[REJECTED] keep the API stable"));

    // structured roundtrip
    let loaded = review.load_report("TASK-001").unwrap();
    assert_eq!(loaded, original);
    assert!(!review.is_clean("TASK-001").unwrap());
}

#[test]
fn clean_report_marks_is_clean_true() {
    let dir = TmpDir::new("clean");
    let (store, git, gh, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let clean = report(
        vec![],
        vec![
            cr("keep the API stable", ConstraintVerdict::Ok),
            cr("sem abstracao nova", ConstraintVerdict::Ok),
        ],
    );
    review.write_report(&clean).unwrap();
    assert!(review.is_clean("TASK-001").unwrap());
}

// --- handoff --------------------------------------------------------------

#[test]
fn handoff_injects_findings_into_session() {
    let dir = TmpDir::new("handoff");
    let (store, git, gh, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let r = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "signature changed".into(),
            reference: Some("RFC-003".into()),
            severity: Severity::Major,
        }],
        vec![cr("keep the API stable", ConstraintVerdict::Failed)],
    );
    review.write_report(&r).unwrap();

    // fake play session over `cat`
    let mut session = ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap();
    let mut reader = session.reader().unwrap();

    review.handoff("TASK-001", &mut session).unwrap();
    session.write_input(&[0x04]).unwrap(); // EOF

    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap();
    assert!(
        buf.contains("src/api.rs:42"),
        "handoff did not inject the findings:\n{buf}"
    );
    assert!(buf.contains("failed constraint: keep the API stable"));
}

#[test]
fn handoff_message_lists_findings_and_rejected_constraints() {
    let r = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "signature changed".into(),
            reference: Some("RFC-003".into()),
            severity: Severity::Major,
        }],
        vec![
            cr("keep the API stable", ConstraintVerdict::Failed),
            cr("sem abstracao nova", ConstraintVerdict::Ok), // ok doesn't enter
        ],
    );
    let msg = jaum_flows::review::handoff_message(&r);
    assert!(msg.contains("fix"));
    assert!(msg.contains("src/api.rs:42"));
    assert!(msg.contains("failed constraint: keep the API stable"));
    assert!(!msg.contains("sem abstracao nova")); // ok doesn't become a pending item
}

// --- structured capture ---------------------------------------------------

#[test]
fn capture_logged_writes_report_and_merges_checklist() {
    use serde_json::json;
    let dir = TmpDir::new("capture");
    let (store, git, gh, docs, repos) = setup(&dir);

    // claude returns a verdict for only ONE constraint; the other must become pending.
    let result = json!({
        "type":"result","subtype":"success","is_error":false,"result":"ok",
        "structured_output": {
            "findings": [
                {"file":"src/api.rs","line":42,"message":"signature changed","reference":"RFC-003"}
            ],
            "constraints": [
                {"text":"keep the API stable","verdict":"failed","note":"signature changed"}
            ]
        }
    });
    let stream = format!("{}\n{}", r#"{"type":"system","subtype":"init"}"#, result);
    let exec = StreamExec(stream);
    let review = Review::new(&store, &git, &gh, &exec, &docs, repos, String::new());

    let mut logs: Vec<String> = Vec::new();
    let report = review
        .capture_logged("TASK-001", &mut |l| logs.push(l.to_string()))
        .unwrap();

    // findings came from claude
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].file, "src/api.rs");
    // checklist is canonical: the 2 `enforce: review` constraints from the fixture
    assert_eq!(report.constraints.len(), 2);
    let api = report
        .constraints
        .iter()
        .find(|c| c.text == "keep the API stable")
        .unwrap();
    assert_eq!(api.verdict, ConstraintVerdict::Failed);
    // the unmentioned one stays pending (jaum guarantees the structure)
    let other = report
        .constraints
        .iter()
        .find(|c| c.text == "sem abstracao nova")
        .unwrap();
    assert_eq!(other.verdict, ConstraintVerdict::Pending);

    assert!(!report.is_clean());
    // actually persisted (roundtrip)
    assert_eq!(review.load_report("TASK-001").unwrap(), report);
    assert!(logs.iter().any(|l| l.contains("review written")));
}

// --- acceptance criteria --------------------------------------------------

const FIXTURE_CRITERIA: &str = r#"---
id: TASK-002
type: impl
status: review
---

## Objective
Fazer X

## Acceptance criteria
- [ ] mostra saldo na tela
- [x] some com o loading
- valida entrada vazia
- [ ] 

## Notas
- this is not a criterion
"#;

#[test]
fn acceptance_checklist_extracts_criteria_from_body() {
    let dir = TmpDir::new("criteria");
    let (store, git, gh, docs, repos) = setup(&dir);
    fs::write(dir.0.join(".backlog/TASK-002.md"), FIXTURE_CRITERIA).unwrap();
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let items = review.acceptance_checklist("TASK-002").unwrap();
    let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
    // checkbox stripped, empty placeholder and "## Notas" item ignored
    assert_eq!(
        texts,
        vec![
            "mostra saldo na tela",
            "some com o loading",
            "valida entrada vazia"
        ]
    );
    assert!(
        items
            .iter()
            .all(|i| i.verdict == ConstraintVerdict::Pending)
    );

    // the review context includes the body and the mandatory criteria checklist
    let ctx = review.build_context("TASK-002").unwrap();
    assert!(ctx.contains("Acceptance criteria to validate"));
    assert!(ctx.contains("mostra saldo na tela"));
}

#[test]
fn is_clean_fails_with_unmet_criterion() {
    let mut r = report(vec![], vec![]);
    r.criteria = vec![cr("mostra saldo", ConstraintVerdict::Failed)];
    assert!(!r.is_clean(), "rejected criterion fails the review");
    r.criteria = vec![cr("mostra saldo", ConstraintVerdict::Pending)];
    assert!(!r.is_clean(), "pending criterion must not pass");
    r.criteria = vec![cr("mostra saldo", ConstraintVerdict::Ok)];
    assert!(r.is_clean(), "all ok -> clean");
}

// --- rendering and counters -------------------------------------------------

#[test]
fn finding_render_covers_all_severity_tags_and_optional_fields() {
    // no line and no reference
    let plain = Finding {
        file: "src/a.rs".into(),
        line: None,
        message: "broken".into(),
        reference: None,
        severity: Severity::Blocker,
    };
    assert_eq!(plain.render(), "[BLOCKER] src/a.rs - broken");

    let minor = finding("style", Severity::Minor);
    assert!(minor.render().starts_with("[MINOR]"));
    let nit = finding("typo", Severity::Nit);
    assert!(nit.render().starts_with("[NIT]"));
}

#[test]
fn unmet_count_counts_failed_and_pending_constraints_and_criteria() {
    let mut r = report(
        vec![],
        vec![
            cr("a", ConstraintVerdict::Ok),
            cr("b", ConstraintVerdict::Failed),
        ],
    );
    r.criteria = vec![
        cr("c", ConstraintVerdict::Pending),
        cr("d", ConstraintVerdict::Ok),
    ];
    assert_eq!(r.unmet_count(), 2);
    assert_eq!(report(vec![], vec![]).unmet_count(), 0);
}

#[test]
fn handoff_message_lists_unmet_criteria() {
    let mut r = report(vec![], vec![]);
    r.criteria = vec![
        cr("mostra saldo", ConstraintVerdict::Failed),
        cr("some com o loading", ConstraintVerdict::Ok), // ok doesn't enter
    ];
    let msg = jaum_flows::review::handoff_message(&r);
    assert!(msg.contains("unmet criterion: mostra saldo"));
    assert!(!msg.contains("some com o loading"));
}

// --- cwd and session flags --------------------------------------------------

/// Recording executor: keeps the (prompt, flags) of every call.
struct Rec {
    calls: RefCell<Vec<(String, ExecFlags)>>,
}
impl Executor for Rec {
    fn spawn_oneshot(&self, p: &str, f: &ExecFlags) -> anyhow::Result<String> {
        self.calls.borrow_mut().push((p.to_string(), f.clone()));
        Ok(String::new())
    }
    fn spawn_interactive(&self, p: &str, f: &ExecFlags) -> anyhow::Result<Session> {
        self.calls.borrow_mut().push((p.to_string(), f.clone()));
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

#[test]
fn review_cwd_falls_back_to_any_repo_then_docs_dir() {
    let dir = TmpDir::new("cwd");
    let (store, git, gh, docs, _repos) = setup(&dir);

    // task repo not mapped: fall back to any mapped repo
    let other = dir.0.join("repos/other");
    let repos = std::collections::HashMap::from([("zzz/other".to_string(), other.clone())]);
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());
    assert_eq!(review.review_cwd("TASK-001"), other);

    // no repos mapped at all (and unknown id): fall back to docs_dir
    let review = Review::new(
        &store,
        &git,
        &gh,
        &FakeExec,
        &docs,
        std::collections::HashMap::new(),
        String::new(),
    );
    assert_eq!(review.review_cwd("TASK-404"), docs);
}

#[test]
fn start_injects_session_id_and_repo_access() {
    let dir = TmpDir::new("startflags");
    let (store, git, gh, docs, repos) = setup(&dir);
    let repo_path = repos["myorg/repo"].clone();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let review = Review::new(&store, &git, &gh, &rec, &docs, repos, String::new());

    let (_session, uuid) = review.start("TASK-001").unwrap();
    assert!(!uuid.is_empty());

    let calls = rec.calls.borrow();
    let (prompt, flags) = &calls[0];
    assert!(prompt.contains("Read-only review of TASK-001"));
    assert_eq!(flags.session_id.as_deref(), Some(uuid.as_str()));
    assert!(flags.resume.is_none());
    assert_eq!(flags.cwd.as_deref(), Some(repo_path.as_path()));
    assert!(flags.extra.iter().any(|x| x == "--add-dir"));
}

#[test]
fn start_without_repos_omits_add_dir_and_uses_docs_cwd() {
    let dir = TmpDir::new("startnorepo");
    let (store, git, gh, docs, _repos) = setup(&dir);
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let review = Review::new(
        &store,
        &git,
        &gh,
        &rec,
        &docs,
        std::collections::HashMap::new(),
        String::new(),
    );

    review.start("TASK-001").unwrap();
    let calls = rec.calls.borrow();
    let (_prompt, flags) = &calls[0];
    assert!(!flags.extra.iter().any(|x| x == "--add-dir"));
    assert_eq!(flags.cwd.as_deref(), Some(docs.as_path()));
}

#[test]
fn resume_reuses_uuid_and_cwd_without_prompt() {
    let dir = TmpDir::new("resume");
    let (store, git, gh, docs, repos) = setup(&dir);
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let review = Review::new(&store, &git, &gh, &rec, &docs, repos, String::new());

    let cwd = dir.0.join("somewhere");
    let _ = review.resume("TASK-001", "uuid-123", &cwd).unwrap();
    let calls = rec.calls.borrow();
    let (prompt, flags) = &calls[0];
    assert!(prompt.is_empty(), "resume must not resend the context");
    assert_eq!(flags.resume.as_deref(), Some("uuid-123"));
    assert!(flags.session_id.is_none());
    assert_eq!(flags.cwd.as_deref(), Some(cwd.as_path()));
}

// --- build_context edge branches --------------------------------------------

const FIXTURE_NO_PR: &str = r#"---
id: TASK-003
type: impl
status: review
prs:
  - repo: myorg/repo
    pr: 0
    branch: feat/task-001
  - repo: other/nope
    pr: 0
    branch: feat/x
  - repo: myorg/repo
    pr: 0
    branch: feat/ghost
---

## Objective
x
"#;

#[test]
fn build_context_falls_back_to_local_diff_and_flags_unmapped_repo() {
    let dir = TmpDir::new("localdiff");
    let (store, git, _gh, docs, repos) = setup(&dir);
    fs::write(dir.0.join(".backlog/TASK-003.md"), FIXTURE_NO_PR).unwrap();
    // gh always fails: PR discovery yields 0 and the local diff is used
    let gh = Gh::with_bin("false");
    let review = Review::new(&store, &git, &gh, &FakeExec, &docs, repos, String::new());

    let ctx = review.build_context("TASK-003").unwrap();
    assert!(ctx.contains("myorg/repo @ feat/task-001 (no PR yet - local diff)"));
    assert!(ctx.contains("(repo other/nope not mapped in the project)"));
    // nonexistent branch: git diff fails and the error is embedded, not propagated
    assert!(ctx.contains("(could not get diff for myorg/repo"));
}

#[test]
fn build_context_reports_missing_docs_repos_and_includes_conventions() {
    let dir = TmpDir::new("emptyctx");
    let (store, git, gh, _docs, _repos) = setup(&dir);
    // nonexistent docs dir + no repos + real conventions
    let missing_docs = dir.0.join("missing-docs");
    let review = Review::new(
        &store,
        &git,
        &gh,
        &FakeExec,
        &missing_docs,
        std::collections::HashMap::new(),
        "- keep it simple",
    );

    let ctx = review.build_context("TASK-001").unwrap();
    assert!(ctx.contains("(no docs found in"));
    assert!(ctx.contains("(no repos mapped in the project)"));
    assert!(ctx.contains("## Project conventions to respect"));
    assert!(ctx.contains("- keep it simple"));
}

// --- capture parse branches --------------------------------------------------

#[test]
fn capture_logged_propagates_claude_error() {
    let dir = TmpDir::new("caperr");
    let (store, git, gh, docs, repos) = setup(&dir);
    let exec = StreamExec(r#"{"type":"result","is_error":true,"result":"usage limit"}"#.into());
    let review = Review::new(&store, &git, &gh, &exec, &docs, repos, String::new());

    let err = review.capture_logged("TASK-001", &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("usage limit"));
}

#[test]
fn capture_logged_without_result_event_fails() {
    let dir = TmpDir::new("capnores");
    let (store, git, gh, docs, repos) = setup(&dir);
    let exec = StreamExec("{\"type\":\"system\",\"subtype\":\"init\"}\n".into());
    let review = Review::new(&store, &git, &gh, &exec, &docs, repos, String::new());

    let err = review.capture_logged("TASK-001", &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("result"));
}

#[test]
fn capture_logged_with_empty_structured_output_keeps_checklist_pending() {
    let dir = TmpDir::new("capempty");
    let (store, git, gh, docs, _repos) = setup(&dir);
    // blank lines interleaved in the stream must be skipped; no repos mapped
    let exec = StreamExec(
        "\n{\"type\":\"result\",\"is_error\":false,\"structured_output\":{}}\n\n".into(),
    );
    let review = Review::new(
        &store,
        &git,
        &gh,
        &exec,
        &docs,
        std::collections::HashMap::new(),
        String::new(),
    );

    let report = review.capture_logged("TASK-001", &mut |_| {}).unwrap();
    assert!(report.findings.is_empty());
    assert_eq!(report.constraints.len(), 2);
    assert!(
        report
            .constraints
            .iter()
            .all(|c| c.verdict == ConstraintVerdict::Pending)
    );
    assert!(!report.is_clean(), "pending checklist keeps it dirty");
}

#[test]
fn capture_logged_merges_criteria_verdicts() {
    use serde_json::json;
    let dir = TmpDir::new("capcrit");
    let (store, git, gh, docs, repos) = setup(&dir);
    fs::write(dir.0.join(".backlog/TASK-002.md"), FIXTURE_CRITERIA).unwrap();

    let result = json!({
        "type":"result","subtype":"success","is_error":false,"result":"ok",
        "structured_output": {
            "findings": [],
            "constraints": [],
            "criteria": [
                {"text":"mostra saldo na tela","verdict":"ok","note":"visible in diff"},
                {"text":"some com o loading","verdict":"failed","note":"spinner remains"}
            ]
        }
    });
    let exec = StreamExec(result.to_string());
    let review = Review::new(&store, &git, &gh, &exec, &docs, repos, String::new());

    let report = review.capture_logged("TASK-002", &mut |_| {}).unwrap();
    let by_text = |t: &str| report.criteria.iter().find(|c| c.text == t).unwrap();
    assert_eq!(
        by_text("mostra saldo na tela").verdict,
        ConstraintVerdict::Ok
    );
    assert_eq!(
        by_text("some com o loading").verdict,
        ConstraintVerdict::Failed
    );
    // the criterion claude did not mention stays pending
    assert_eq!(
        by_text("valida entrada vazia").verdict,
        ConstraintVerdict::Pending
    );
    assert!(!report.is_clean());
}
