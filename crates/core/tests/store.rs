use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_core::{Enforce, JaumError, Status, Store, TaskType};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Diretório temporário com limpeza automática no drop — evita dependência
/// externa (tempfile) só para testes.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-test-{tag}-{}-{n}", std::process::id()));
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
id: TASK-012
type: impl
status: wip
rfcs: [RFC-003, RFC-007]
adrs: [ADR-011]
prs:
  - repo: tono-lang/parser
    pr: 142
    branch: feat/task-012
  - repo: tono-lang/runtime
    pr: 0
    branch: feat/task-012
deferred:
  - "primitivo decimal fica pra TASK-024"
constraints:
  - text: "nao tocar em src/legacy/"
    enforce: hook
  - text: "nao rodar migration"
    enforce: hook
  - text: "manter API estavel"
    enforce: review
  - text: "sem abstracao nova"
    enforce: review
---

## Objetivo
Implementar enum aberto.

## Criterio de aceite
- [ ] enum aberto preserva variante desconhecida (RFC-003)
"#;

fn seed_fixture(dir: &TmpDir) -> Store {
    let store = Store::new(&dir.0);
    fs::write(dir.0.join("TASK-012.md"), FIXTURE).unwrap();
    store
}

#[test]
fn parse_le_todos_os_campos_do_frontmatter() {
    let dir = TmpDir::new("parse");
    let store = seed_fixture(&dir);
    let t = store.get("TASK-012").unwrap();

    assert_eq!(t.id, "TASK-012");
    assert_eq!(t.task_type, TaskType::Impl);
    assert_eq!(t.status, Status::Wip);
    assert_eq!(t.rfcs, vec!["RFC-003", "RFC-007"]);
    assert_eq!(t.adrs, vec!["ADR-011"]);
    assert_eq!(t.prs.len(), 2);
    assert_eq!(t.prs[0].repo, "tono-lang/parser");
    assert_eq!(t.prs[0].pr, 142);
    assert_eq!(t.prs[1].pr, 0); // ainda não criado
    assert_eq!(t.deferred, vec!["primitivo decimal fica pra TASK-024"]);
    assert_eq!(t.constraints.len(), 4);
    assert!(
        t.body
            .contains("enum aberto preserva variante desconhecida")
    );
    assert_eq!(t.path, Some(dir.0.join("TASK-012.md")));
}

#[test]
fn constraints_filtradas_por_enforce() {
    let dir = TmpDir::new("constraints");
    let store = seed_fixture(&dir);

    let hooks = store.constraints("TASK-012", Enforce::Hook).unwrap();
    assert_eq!(hooks.len(), 2);
    assert!(hooks.iter().all(|c| c.enforce == Enforce::Hook));
    assert!(hooks.iter().any(|c| c.text == "nao tocar em src/legacy/"));

    let reviews = store.constraints("TASK-012", Enforce::Review).unwrap();
    assert_eq!(reviews.len(), 2);
    assert!(reviews.iter().any(|c| c.text == "manter API estavel"));
}

#[test]
fn linked_repos_deduplica_preservando_ordem() {
    let dir = TmpDir::new("repos");
    let store = seed_fixture(&dir);
    let repos = store.linked_repos("TASK-012").unwrap();
    assert_eq!(repos, vec!["tono-lang/parser", "tono-lang/runtime"]);
}

#[test]
fn write_e_parse_sao_roundtrip() {
    let dir = TmpDir::new("roundtrip");
    let store = seed_fixture(&dir);
    let original = store.get("TASK-012").unwrap();

    // regrava e relê: campos de domínio preservados
    store.write(&original).unwrap();
    let again = store.get("TASK-012").unwrap();

    assert_eq!(original.id, again.id);
    assert_eq!(original.status, again.status);
    assert_eq!(original.task_type, again.task_type);
    assert_eq!(original.rfcs, again.rfcs);
    assert_eq!(original.prs, again.prs);
    assert_eq!(original.constraints, again.constraints);
    assert_eq!(original.deferred, again.deferred);
    assert_eq!(original.body, again.body);
}

#[test]
fn list_filtra_por_status() {
    let dir = TmpDir::new("list");
    let store = seed_fixture(&dir);
    store.create_stub(&["RFC-001".to_string()]).unwrap(); // backlog

    let all = store.list(None).unwrap();
    assert_eq!(all.len(), 2);

    let wip = store.list(Some(Status::Wip)).unwrap();
    assert_eq!(wip.len(), 1);
    assert_eq!(wip[0].id, "TASK-012");

    let backlog = store.list(Some(Status::Backlog)).unwrap();
    assert_eq!(backlog.len(), 1);
}

#[test]
fn list_ignora_arquivos_de_review() {
    let dir = TmpDir::new("review-ignore");
    let store = seed_fixture(&dir);
    fs::write(dir.0.join("TASK-012.review.md"), "# findings").unwrap();

    let all = store.list(None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "TASK-012");
}

#[test]
fn get_inexistente_devolve_task_not_found() {
    let dir = TmpDir::new("notfound");
    let store = Store::new(&dir.0);
    let err = store.get("TASK-999").unwrap_err();
    assert_eq!(
        err.downcast_ref::<JaumError>(),
        Some(&JaumError::TaskNotFound("TASK-999".to_string()))
    );
}

#[test]
fn create_stub_gera_id_sequencial_e_backlog() {
    let dir = TmpDir::new("stub");
    let store = seed_fixture(&dir); // já tem TASK-012

    let stub = store.create_stub(&["RFC-009".to_string()]).unwrap();
    assert_eq!(stub.id, "TASK-013");
    assert_eq!(stub.status, Status::Backlog);
    assert_eq!(stub.task_type, TaskType::Impl);
    assert_eq!(stub.rfcs, vec!["RFC-009"]);

    // persistiu de fato
    assert_eq!(store.get("TASK-013").unwrap().rfcs, vec!["RFC-009"]);
}

#[test]
fn set_status_persiste() {
    let dir = TmpDir::new("setstatus");
    let store = seed_fixture(&dir);
    store.set_status("TASK-012", Status::Review).unwrap();
    assert_eq!(store.get("TASK-012").unwrap().status, Status::Review);
}

#[test]
fn set_pr_atualiza_numero_do_repo_certo() {
    let dir = TmpDir::new("setpr");
    let store = seed_fixture(&dir);

    store.set_pr("TASK-012", "tono-lang/runtime", 200).unwrap();
    let t = store.get("TASK-012").unwrap();
    let runtime = t
        .prs
        .iter()
        .find(|p| p.repo == "tono-lang/runtime")
        .unwrap();
    assert_eq!(runtime.pr, 200);
    // o outro repo não muda
    let parser = t.prs.iter().find(|p| p.repo == "tono-lang/parser").unwrap();
    assert_eq!(parser.pr, 142);
}

#[test]
fn set_pr_em_repo_nao_linkado_falha() {
    let dir = TmpDir::new("setpr-fail");
    let store = seed_fixture(&dir);
    let err = store.set_pr("TASK-012", "outro/repo", 1).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<JaumError>(),
        Some(JaumError::PrLinkNotFound { .. })
    ));
}

#[test]
fn add_deferred_registra_e_cria_novo_backlog() {
    let dir = TmpDir::new("defer");
    let store = seed_fixture(&dir);

    let spawned = store
        .add_deferred("TASK-012", "extrair parser de datas")
        .unwrap();

    // registrado na origem
    let origin = store.get("TASK-012").unwrap();
    assert!(
        origin
            .deferred
            .contains(&"extrair parser de datas".to_string())
    );

    // novo backlog materializado, referenciando a origem
    assert_eq!(spawned.id, "TASK-013");
    assert_eq!(spawned.status, Status::Backlog);
    assert!(spawned.body.contains("extrair parser de datas"));
    assert!(spawned.body.contains("Derivado de TASK-012"));
}
