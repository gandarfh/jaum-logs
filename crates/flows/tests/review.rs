use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Git, Session};
use jaum_core::Store;
use jaum_flows::review::{
    ConstraintResult, ConstraintVerdict, Finding, Review, ReviewReport, read_only_flags,
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
  - text: "nao tocar em src/legacy/"
    enforce: hook
  - text: "manter API estavel"
    enforce: review
  - text: "sem abstracao nova"
    enforce: review
---

## Objetivo
x
"#;

/// Executor de mentira: spawn_interactive devolve sessão real sobre `cat`.
struct FakeExec;
impl Executor for FakeExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

fn setup(dir: &TmpDir) -> (Store, Git, PathBuf, PathBuf) {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();
    let docs = dir.0.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("RFC-003.md"), "# RFC-003\nenum aberto\n").unwrap();
    fs::write(docs.join("ADR-011.md"), "# ADR-011\ndecisao\n").unwrap();
    let repos = dir.0.join("repos");
    git_init(&repos.join("repo"));
    (Store::new(&backlog), Git::new(), docs, repos)
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
fn read_only_flags_bloqueia_toda_escrita() {
    let f = read_only_flags();
    for t in ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"] {
        assert!(f.disallowed_tools.iter().any(|x| x == t), "faltou bloquear {t}");
    }
}

// --- is_clean (puro) ------------------------------------------------------

fn report(findings: Vec<Finding>, constraints: Vec<ConstraintResult>) -> ReviewReport {
    ReviewReport {
        task: "TASK-001".into(),
        findings,
        constraints,
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
fn is_clean_so_com_zero_findings_e_todas_ok() {
    let limpo = report(vec![], vec![cr("manter API estavel", ConstraintVerdict::Ok)]);
    assert!(limpo.is_clean());
}

#[test]
fn is_clean_falha_com_finding() {
    let r = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "mudou assinatura".into(),
            reference: Some("RFC-003".into()),
        }],
        vec![cr("manter API estavel", ConstraintVerdict::Ok)],
    );
    assert!(!r.is_clean());
}

#[test]
fn is_clean_falha_com_constraint_reprovada_ou_pendente() {
    let reprovada = report(vec![], vec![cr("x", ConstraintVerdict::Reprovado)]);
    assert!(!reprovada.is_clean());
    let pendente = report(vec![], vec![cr("x", ConstraintVerdict::Pending)]);
    assert!(!pendente.is_clean(), "pendente não pode passar");
}

// --- contexto e checklist -------------------------------------------------

#[test]
fn check_semantic_constraints_so_pega_enforce_review_como_pending() {
    let dir = TmpDir::new("checklist");
    let (store, git, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &FakeExec, &docs, &repos);

    let items = review.check_semantic_constraints("TASK-001").unwrap();
    assert_eq!(items.len(), 2); // só as enforce: review
    assert!(items.iter().all(|i| i.verdict == ConstraintVerdict::Pending));
    assert!(items.iter().any(|i| i.text == "manter API estavel"));
    // a enforce: hook NÃO entra
    assert!(!items.iter().any(|i| i.text.contains("src/legacy")));
}

#[test]
fn build_context_traz_docs_diff_e_checklist() {
    let dir = TmpDir::new("context");
    let (store, git, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &FakeExec, &docs, &repos);

    let ctx = review.build_context("TASK-001").unwrap();
    assert!(ctx.contains("RFC-003")); // todos os docs
    assert!(ctx.contains("ADR-011"));
    assert!(ctx.contains("Diff dos PRs"));
    assert!(ctx.contains("myorg/repo"));
    assert!(ctx.contains("manter API estavel")); // checklist
    assert!(ctx.contains("sem abstracao nova"));
}

// --- persistência ---------------------------------------------------------

#[test]
fn write_e_load_report_roundtrip() {
    let dir = TmpDir::new("persist");
    let (store, git, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &FakeExec, &docs, &repos);

    let original = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "mudou assinatura".into(),
            reference: Some("RFC-003".into()),
        }],
        vec![
            cr("manter API estavel", ConstraintVerdict::Reprovado),
            cr("sem abstracao nova", ConstraintVerdict::Ok),
        ],
    );
    review.write_report(&original).unwrap();

    // arquivo no lugar certo e legível
    let path = dir.0.join(".backlog/TASK-001.review.md");
    assert!(path.exists());
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("src/api.rs:42 — mudou assinatura (viola RFC-003)"));
    assert!(raw.contains("[REPROVADO] manter API estavel"));

    // roundtrip estruturado
    let loaded = review.load_report("TASK-001").unwrap();
    assert_eq!(loaded, original);
    assert!(!review.is_clean("TASK-001").unwrap());
}

#[test]
fn report_limpo_marca_is_clean_true() {
    let dir = TmpDir::new("clean");
    let (store, git, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &FakeExec, &docs, &repos);

    let limpo = report(
        vec![],
        vec![
            cr("manter API estavel", ConstraintVerdict::Ok),
            cr("sem abstracao nova", ConstraintVerdict::Ok),
        ],
    );
    review.write_report(&limpo).unwrap();
    assert!(review.is_clean("TASK-001").unwrap());
}

// --- handoff --------------------------------------------------------------

#[test]
fn handoff_injeta_findings_na_sessao() {
    let dir = TmpDir::new("handoff");
    let (store, git, docs, repos) = setup(&dir);
    let review = Review::new(&store, &git, &FakeExec, &docs, &repos);

    let r = report(
        vec![Finding {
            file: "src/api.rs".into(),
            line: Some(42),
            message: "mudou assinatura".into(),
            reference: Some("RFC-003".into()),
        }],
        vec![cr("manter API estavel", ConstraintVerdict::Reprovado)],
    );
    review.write_report(&r).unwrap();

    // sessão de play de mentira sobre `cat`
    let mut session = ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap();
    let mut reader = session.reader().unwrap();

    review.handoff("TASK-001", &mut session).unwrap();
    session.write_input(&[0x04]).unwrap(); // EOF

    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("src/api.rs:42"), "handoff não injetou os findings:\n{buf}");
    assert!(buf.contains("constraint reprovada: manter API estavel"));
}
