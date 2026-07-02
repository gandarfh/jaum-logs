use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Session};
use jaum_core::{Store, TaskType};
use jaum_flows::ingest::{
    Ingest, ProposedTask, create_stubs, parse_stream, parse_structured, schema, summarize_event,
};

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
    let scan = parse_structured(ENVELOPE).unwrap();
    assert_eq!(scan.tasks.len(), 2);
    assert_eq!(scan.tasks[0].title, "IR de tipos");
    assert_eq!(scan.tasks[0].rfcs, vec!["rfc-0002"]);
    assert_eq!(scan.tasks[1].task_type, "spike");
}

#[test]
fn parse_structured_extrai_docs_classificados() {
    let env = r#"{"is_error":false,"result":"ok","structured_output":{
        "tasks":[],
        "docs":[
            {"path":"/abs/personal-docs/0001-foo.md","kind":"adr","name":"ADR-0001-foo.md"},
            {"path":"/abs/personal-docs/rfc.md","kind":"rfc","name":"RFC-0005-bar.md"}
        ]
    }}"#;
    let scan = parse_structured(env).unwrap();
    assert!(scan.tasks.is_empty());
    assert_eq!(scan.docs.len(), 2);
    assert_eq!(scan.docs[0].kind, "adr");
    assert_eq!(scan.docs[0].name, "ADR-0001-foo.md");
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

/// Saída `stream-json --verbose`: um evento JSON por linha, `result` no fim.
const STREAM: &str = r#"{"type":"system","subtype":"init","session_id":"x"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Encontrei o RFC-0002 sobre o IR de tipos"}]}}
{"type":"user","message":{"content":[{"type":"tool_result"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"ok","structured_output":{"tasks":[{"title":"IR de tipos","type":"impl","rfcs":["rfc-0002"],"objetivo":"Implementar o IR"}]}}"#;

#[test]
fn parse_stream_pega_o_result_final() {
    let scan = parse_stream(STREAM).unwrap();
    assert_eq!(scan.tasks.len(), 1);
    assert_eq!(scan.tasks[0].title, "IR de tipos");
    assert_eq!(scan.tasks[0].rfcs, vec!["rfc-0002"]);
}

#[test]
fn parse_stream_sem_result_falha() {
    let only_events = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[]}}"#;
    assert!(parse_stream(only_events).is_err());
}

#[test]
fn summarize_event_resume_eventos_relevantes() {
    // init traz o modelo
    assert_eq!(
        summarize_event(r#"{"type":"system","subtype":"init","model":"opus-4"}"#),
        vec!["sessão iniciada · modelo opus-4"]
    );
    // result básico
    assert_eq!(
        summarize_event(r#"{"type":"result","subtype":"success"}"#),
        vec!["concluído"]
    );
    // eventos sem interesse não viram linha
    assert!(summarize_event(r#"{"type":"rate_limit_event"}"#).is_empty());
    assert!(summarize_event("nao json").is_empty());
}

#[test]
fn summarize_event_inclui_argumentos_das_tools() {
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"crates/flows/src/ingest.rs"}}]}}"#;
    assert_eq!(summarize_event(read), vec!["→ Read crates/flows/src/ingest.rs"]);

    let bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git status"}}]}}"#;
    assert_eq!(summarize_event(bash), vec!["→ Bash git status"]);

    let task = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Task","input":{"subagent_type":"Explore","description":"mapear docs"}}]}}"#;
    assert_eq!(summarize_event(task), vec!["→ Task (Explore) mapear docs"]);

    // texto vem com o modelo e múltiplos blocos viram múltiplas linhas
    let multi = r#"{"type":"assistant","message":{"model":"sonnet","content":[{"type":"text","text":"Achei dois RFCs"},{"type":"tool_use","name":"Grep","input":{"pattern":"RFC-","path":"docs"}}]}}"#;
    assert_eq!(
        summarize_event(multi),
        vec!["[sonnet] Achei dois RFCs", "→ Grep RFC-  em docs"]
    );
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

/// Executor fake que devolve uma saída `stream-json` fixa (com o `result` final).
struct StreamExec(String);
impl Executor for StreamExec {
    fn spawn_oneshot(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

#[test]
fn run_logged_organiza_docs_por_categoria() {
    let dir = TmpDir::new("mirror");
    // repo com um arquivo cujo NOME parece numérico mas é um ADR pelo conteúdo
    let repo = dir.0.join("personal-docs");
    fs::create_dir_all(&repo).unwrap();
    let doc = repo.join("0001-protocolo-como-anotacao.md");
    fs::write(&doc, "# ADR-0001\nprotocolo como anotação\n").unwrap();
    let doc_abs = fs::canonicalize(&doc).unwrap();

    let docs_dir = dir.0.join("ext-docs");
    fs::create_dir_all(&docs_dir).unwrap();
    let store = Store::new(dir.0.join(".backlog"));

    // o agente classifica como adr e propõe nome canônico
    let stream = format!(
        "{}\n{}",
        r#"{"type":"system","subtype":"init","model":"opus"}"#,
        serde_json::json!({
            "type":"result","subtype":"success","is_error":false,"result":"ok",
            "structured_output":{"tasks":[],"docs":[
                {"path": doc_abs.to_string_lossy(), "kind":"adr", "name":"ADR-0001-protocolo-como-anotacao.md"}
            ]}
        })
    );
    let exec = StreamExec(stream);
    let ingest = Ingest::new(&store, &exec, docs_dir.clone(), vec![repo.clone()]);

    let mut logs: Vec<String> = Vec::new();
    let outcome = ingest
        .run_logged(&mut |l| logs.push(l.to_string()))
        .unwrap();

    assert_eq!(outcome.docs_imported, 1);
    // organizado em docs_dir/adrs/<nome-canônico>
    let mirrored = docs_dir.join("adrs/ADR-0001-protocolo-como-anotacao.md");
    assert!(mirrored.exists(), "doc não foi organizado em {mirrored:?}");
    assert!(
        fs::read_to_string(&mirrored).unwrap().contains("protocolo como anotação")
    );
    assert!(logs.iter().any(|l| l.contains("espelhados")));
}

#[test]
fn run_logged_move_doc_solto_no_docs_dir_para_categoria() {
    let dir = TmpDir::new("organize");
    let docs_dir = dir.0.join("ext-docs");
    fs::create_dir_all(&docs_dir).unwrap();
    // doc escrito solto na raiz do docs_dir (caso slyde)
    let loose = docs_dir.join("PRD-00-overview.md");
    fs::write(&loose, "# PRD-00\nvisão\n").unwrap();
    let loose_abs = fs::canonicalize(&loose).unwrap();
    let store = Store::new(dir.0.join(".backlog"));

    let stream = format!(
        "{}\n{}",
        r#"{"type":"system","subtype":"init"}"#,
        serde_json::json!({
            "type":"result","subtype":"success","is_error":false,"result":"ok",
            "structured_output":{"tasks":[],"docs":[
                {"path": loose_abs.to_string_lossy(), "kind":"prd", "name":"PRD-00-overview.md"}
            ]}
        })
    );
    let exec = StreamExec(stream);
    let ingest = Ingest::new(&store, &exec, docs_dir.clone(), Vec::new());
    let outcome = ingest.run_logged(&mut |_| {}).unwrap();

    assert_eq!(outcome.docs_imported, 1);
    // foi MOVIDO para prd/ (original removido, sem duplicar)
    assert!(docs_dir.join("prd/PRD-00-overview.md").exists());
    assert!(!loose.exists(), "original solto deveria ter sido movido");
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
