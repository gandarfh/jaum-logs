use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Session};
use jaum_core::Store;
use jaum_flows::setup::{Setup, branch_leaks_id, is_template};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-setup-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FakeExec;
impl Executor for FakeExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

#[test]
fn branch_leaks_id_pega_padrao_da_task() {
    assert!(branch_leaks_id("feat/task-001"));
    assert!(branch_leaks_id("TASK-12-fix"));
    assert!(branch_leaks_id("chore/refactor-task-3"));
    // branches que descrevem o trabalho não disparam
    assert!(!branch_leaks_id("feat/markdown-deck-parser"));
    assert!(!branch_leaks_id("fix/task-runner-timeout")); // "task-runner", sem dígito
}

#[test]
fn is_template_detecta_vazio_e_scaffold() {
    assert!(is_template(""));
    assert!(is_template(
        "# Convenções\n\nUma por linha (use `-`).\n\n- \n"
    ));
    assert!(!is_template("# Convenções\n\n- não referenciar nº de RFC em comentários\n"));
}

#[test]
fn build_prompt_traz_tasks_repos_e_setup_obrigatorio() {
    let dir = TmpDir::new("prompt");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    // task sem prs (como nasce do ingest)
    fs::write(
        backlog.join("TASK-001.md"),
        "---\nid: TASK-001\ntype: impl\nstatus: backlog\nrfcs: [RFC-0001]\n---\n\n## Objetivo\nimplementar o parser\n",
    )
    .unwrap();
    let store = Store::new(&backlog);
    let repos = HashMap::from([("org/app".to_string(), dir.0.join("repo"))]);

    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), repos, "");
    let p = setup.build_prompt().unwrap();

    assert!(p.contains("Contrato")); // declara o contrato positivamente
    assert!(p.contains("Seu trabalho"));
    assert!(p.contains("TASK-001"));
    assert!(p.contains("SEM repo")); // sinaliza que falta vincular
    assert!(p.contains("org/app")); // slug disponível
    assert!(p.contains("template")); // conventions vazio
    assert!(p.contains("prs")); // schema de vínculo
}

#[test]
fn build_prompt_sinaliza_branch_com_id_vazado() {
    let dir = TmpDir::new("leak");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(
        backlog.join("TASK-001.md"),
        "---\nid: TASK-001\ntype: impl\nstatus: backlog\nprs:\n  - repo: org/app\n    pr: 0\n    branch: feat/task-001\n---\n\n## Objetivo\nx\n",
    )
    .unwrap();
    let store = Store::new(&backlog);
    let repos = HashMap::from([("org/app".to_string(), dir.0.join("repo"))]);
    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), repos, "");
    let p = setup.build_prompt().unwrap();
    assert!(p.contains("vaza o id"));
}

#[test]
fn start_abre_sessao_interativa() {
    let dir = TmpDir::new("start");
    let backlog = dir.0.join("backlog");
    fs::create_dir_all(&backlog).unwrap();
    let store = Store::new(&backlog);
    let setup = Setup::new(&store, &FakeExec, dir.0.clone(), HashMap::new(), "");
    // não deve panicar montando a sessão (cat como executor de mentira)
    let (mut s, uuid) = setup.start().unwrap();
    assert!(!uuid.is_empty(), "start deve devolver um session-id");
    s.write_input(&[0x04]).unwrap();
    assert!(s.wait().unwrap());
}
