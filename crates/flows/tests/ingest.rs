use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_core::{Store, TaskType};
use jaum_flows::ingest::{ProposedTask, create_stubs, parse_structured, schema};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-ingest-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Envelope como o `claude --output-format json --json-schema` devolve.
const ENVELOPE: &str = r#"{
  "type":"result","subtype":"success","is_error":false,
  "result":"ok",
  "structured_output":{
    "tasks":[
      {"title":"IR de tipos","type":"impl","rfcs":["rfc-0002"],"objetivo":"Implementar o IR de tipos","criterio":["enum aberto","nullability"]},
      {"title":"Explorar serializacao","type":"spike","rfcs":["RFC-0015"],"adrs":["ADR-3"],"objetivo":"Investigar formato"}
    ]
  }
}"#;

#[test]
fn parse_structured_extrai_tasks_do_envelope() {
    let tasks = parse_structured(ENVELOPE).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].title, "IR de tipos");
    assert_eq!(tasks[0].rfcs, vec!["rfc-0002"]);
    assert_eq!(tasks[1].task_type, "spike");
}

#[test]
fn parse_structured_propaga_is_error() {
    let env = r#"{"is_error":true,"result":"limite de uso"}"#;
    let err = parse_structured(env).unwrap_err();
    assert!(err.to_string().contains("limite de uso"));
}

#[test]
fn parse_structured_sem_structured_output_falha() {
    let env = r#"{"is_error":false,"result":"texto solto"}"#;
    assert!(parse_structured(env).is_err());
}

#[test]
fn schema_exige_tasks() {
    let s = schema();
    assert_eq!(s["required"][0], "tasks");
    assert_eq!(s["properties"]["tasks"]["type"], "array");
}

fn proposed(title: &str, ty: &str, rfcs: &[&str]) -> ProposedTask {
    ProposedTask {
        title: title.into(),
        task_type: ty.into(),
        rfcs: rfcs.iter().map(|s| s.to_string()).collect(),
        adrs: Vec::new(),
        objetivo: "fazer X".into(),
        criterio: vec!["criterio 1".into()],
    }
}

#[test]
fn create_stubs_materializa_backlog_normalizando_refs() {
    let dir = TmpDir::new("create");
    let store = Store::new(dir.0.join(".backlog"));

    let created = create_stubs(
        &store,
        &[
            proposed("IR tipos", "impl", &["rfc-0002"]),
            proposed("Serializacao", "spike", &["rfc-0015"]),
        ],
    )
    .unwrap();

    assert_eq!(created.len(), 2);
    assert_eq!(created[0].id, "TASK-001");
    // refs normalizadas para maiúsculas
    assert_eq!(created[0].rfcs, vec!["RFC-0002"]);
    assert_eq!(created[0].task_type, TaskType::Impl);
    assert_eq!(created[1].task_type, TaskType::Spike);
    // corpo contém objetivo e critério
    assert!(created[0].body.contains("fazer X"));
    assert!(created[0].body.contains("- [ ] criterio 1"));

    // persistido
    assert_eq!(store.list(None).unwrap().len(), 2);
}

#[test]
fn create_stubs_deduplica_por_refs_em_reexecucao() {
    let dir = TmpDir::new("dedup");
    let store = Store::new(dir.0.join(".backlog"));

    create_stubs(&store, &[proposed("IR tipos", "impl", &["rfc-0002"])]).unwrap();
    // re-executar com a mesma ref (case diferente) não duplica
    let again = create_stubs(&store, &[proposed("IR tipos again", "impl", &["RFC-0002"])]).unwrap();
    assert!(again.is_empty(), "não deveria recriar a mesma ref");
    assert_eq!(store.list(None).unwrap().len(), 1);
}
