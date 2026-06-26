use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Git, Session};
use jaum_core::{Status, Store, Task};
use jaum_flows::play::{Play, build_prompt, guard_flags, pretool_hook_script, reinjection_text};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-play-{tag}-{}-{n}", std::process::id()));
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
status: ready
rfcs: [RFC-003]
adrs: [ADR-011]
prs:
  - repo: myorg/repo
    pr: 0
    branch: feat/task-001
constraints:
  - text: "nao tocar em src/legacy/"
    enforce: hook
  - text: "nao rodar migration"
    enforce: hook
  - text: "manter API estavel"
    enforce: review
---

## Objetivo
Implementar enum aberto.

## Criterio de aceite
- [ ] feito
"#;

fn task() -> Task {
    let dir = TmpDir::new("task");
    let store = Store::new(&dir.0);
    fs::write(dir.0.join("TASK-001.md"), FIXTURE).unwrap();
    store.get("TASK-001").unwrap()
}

// --- partes puras ---------------------------------------------------------

#[test]
fn build_prompt_inclui_objetivo_contexto_e_constraints() {
    let p = build_prompt(&task());
    assert!(p.contains("Implementar enum aberto"));
    assert!(p.contains("RFC-003"));
    assert!(p.contains("ADR-011"));
    assert!(p.contains("manter API estavel")); // enforce: review aparece no corpo
    assert!(p.contains("NUNCA faça merge"));
}

#[test]
fn guard_flags_bloqueia_merge_e_injeta_constraints() {
    let f = guard_flags(&task());
    assert!(f.disallowed_tools.iter().any(|t| t.contains("git merge")));
    assert!(f.disallowed_tools.iter().any(|t| t.contains("gh pr merge")));
    let sys = f.append_system_prompt.unwrap();
    assert!(sys.contains("nao tocar em src/legacy/"));
    assert!(sys.contains("manter API estavel"));
}

#[test]
fn reinjection_separa_mecanicas_de_semanticas() {
    let t = reinjection_text(&task());
    assert!(t.contains("Bloqueadas mecanicamente"));
    assert!(t.contains("nao rodar migration"));
    assert!(t.contains("Sua responsabilidade"));
    assert!(t.contains("manter API estavel"));
}

// --- execução real do PreToolUse hook (patch 1) ---------------------------

fn run_hook(script: &str, stdin_json: &str) -> String {
    let dir = TmpDir::new("hook");
    let path = dir.0.join("pretool.sh");
    fs::write(&path, script).unwrap();
    let mut child = Command::new("bash")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn hook_bloqueia_merge_sempre() {
    let s = pretool_hook_script(&task());
    let out = run_hook(&s, r#"{"tool_name":"Bash","tool_input":{"command":"git merge main"}}"#);
    assert!(out.contains("\"permissionDecision\":\"deny\""), "out: {out}");
    assert!(out.contains("merge"));
}

#[test]
fn hook_bloqueia_constraint_de_caminho() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/legacy/foo.rs"}}"#,
    );
    assert!(out.contains("deny"), "deveria bloquear src/legacy/; out: {out}");
    assert!(out.contains("src/legacy/"));
}

#[test]
fn hook_bloqueia_constraint_de_keyword() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Bash","tool_input":{"command":"npm run migration"}}"#,
    );
    assert!(out.contains("deny"), "deveria bloquear migration; out: {out}");
}

#[test]
fn hook_libera_acao_que_nao_casa_constraint() {
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/main.rs"}}"#,
    );
    assert!(out.trim().is_empty(), "não deveria bloquear src/main.rs; out: {out}");
}

#[test]
fn hook_nao_bloqueia_constraint_enforce_review() {
    // "manter API estavel" é enforce: review -> hook NÃO pega (é detectivo no review)
    let s = pretool_hook_script(&task());
    let out = run_hook(
        &s,
        r#"{"tool_name":"Edit","tool_input":{"file_path":"src/api.rs"}}"#,
    );
    assert!(out.trim().is_empty(), "review constraints não entram no hook; out: {out}");
}

// --- start (executor de mentira sobre `cat` + git fixture) ----------------

struct Rec {
    calls: RefCell<Vec<(String, ExecFlags)>>,
}
impl Executor for Rec {
    fn spawn_oneshot(&self, prompt: &str, flags: &ExecFlags) -> anyhow::Result<String> {
        self.calls.borrow_mut().push((prompt.to_string(), flags.clone()));
        Ok(String::new())
    }
    fn spawn_interactive(&self, prompt: &str, flags: &ExecFlags) -> anyhow::Result<Session> {
        self.calls.borrow_mut().push((prompt.to_string(), flags.clone()));
        // sessão real sobre `cat` só para devolver um Session válido
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
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
}

#[test]
fn start_cria_worktree_instala_hooks_e_marca_wip() {
    let root = TmpDir::new("start");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    fs::write(backlog.join("TASK-001.md"), FIXTURE).unwrap();
    let repos_root = root.0.join("repos");
    git_init(&repos_root.join("repo"));

    let store = Store::new(&backlog);
    let git = Git::new();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let play = Play::new(&store, &git, &rec, root.0.join(".jaum"), &repos_root);

    let mut ps = play.start("TASK-001").unwrap();

    // worktree e artefatos no disco
    assert!(ps.worktrees[0].1.exists(), "worktree não criada");
    assert!(ps.artifacts.settings_path.exists());
    assert!(ps.artifacts.pretool_path.exists());

    // status virou wip
    assert_eq!(store.get("TASK-001").unwrap().status, Status::Wip);

    // flags e prompt corretos chegaram ao executor
    let calls = rec.calls.borrow();
    let (prompt, flags) = &calls[0];
    assert!(prompt.contains("Implementar enum aberto"));
    assert!(flags.disallowed_tools.iter().any(|t| t.contains("git merge")));
    assert!(flags.settings.is_some(), "hook (--settings) não aplicado");
    assert!(flags.cwd.is_some(), "cwd (worktree) não aplicada");
    drop(calls);

    play.stop(&mut ps).unwrap();
    assert!(!ps.worktrees[0].1.exists(), "worktree não removida no stop");
}

#[test]
fn start_recusa_spike() {
    let root = TmpDir::new("spike");
    let backlog = root.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    let spike = FIXTURE.replace("type: impl", "type: spike");
    fs::write(backlog.join("TASK-001.md"), spike).unwrap();

    let store = Store::new(&backlog);
    let git = Git::new();
    let rec = Rec {
        calls: RefCell::new(Vec::new()),
    };
    let play = Play::new(&store, &git, &rec, root.0.join(".jaum"), root.0.join("repos"));

    let err = match play.start("TASK-001") {
        Ok(_) => panic!("deveria recusar spike"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("spike"));
}
