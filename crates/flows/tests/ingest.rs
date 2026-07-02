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

/// Envelope as returned by `claude --output-format json --json-schema`.
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
fn parse_structured_extracts_tasks_from_envelope() {
    let scan = parse_structured(ENVELOPE).unwrap();
    assert_eq!(scan.tasks.len(), 2);
    assert_eq!(scan.tasks[0].title, "IR de tipos");
    assert_eq!(scan.tasks[0].rfcs, vec!["rfc-0002"]);
    assert_eq!(scan.tasks[1].task_type, "spike");
}

#[test]
fn parse_structured_extracts_classified_docs() {
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
fn parse_structured_propagates_is_error() {
    let env = r#"{"is_error":true,"result":"limite de uso"}"#;
    let err = parse_structured(env).unwrap_err();
    assert!(err.to_string().contains("limite de uso"));
}

#[test]
fn parse_structured_without_structured_output_fails() {
    let env = r#"{"is_error":false,"result":"texto solto"}"#;
    assert!(parse_structured(env).is_err());
}

/// `stream-json --verbose` output: one JSON event per line, `result` at the end.
const STREAM: &str = r#"{"type":"system","subtype":"init","session_id":"x"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Encontrei o RFC-0002 sobre o IR de tipos"}]}}
{"type":"user","message":{"content":[{"type":"tool_result"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"ok","structured_output":{"tasks":[{"title":"IR de tipos","type":"impl","rfcs":["rfc-0002"],"objetivo":"Implementar o IR"}]}}"#;

#[test]
fn parse_stream_takes_final_result() {
    let scan = parse_stream(STREAM).unwrap();
    assert_eq!(scan.tasks.len(), 1);
    assert_eq!(scan.tasks[0].title, "IR de tipos");
    assert_eq!(scan.tasks[0].rfcs, vec!["rfc-0002"]);
}

#[test]
fn parse_stream_without_result_fails() {
    let only_events = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[]}}"#;
    assert!(parse_stream(only_events).is_err());
}

#[test]
fn summarize_event_summarizes_relevant_events() {
    // init carries the model
    assert_eq!(
        summarize_event(r#"{"type":"system","subtype":"init","model":"opus-4"}"#),
        vec!["session started · model opus-4"]
    );
    // basic result
    assert_eq!(
        summarize_event(r#"{"type":"result","subtype":"success"}"#),
        vec!["done"]
    );
    // uninteresting events don't become a line
    assert!(summarize_event(r#"{"type":"rate_limit_event"}"#).is_empty());
    assert!(summarize_event("not json").is_empty());
}

#[test]
fn summarize_event_includes_tool_arguments() {
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"crates/flows/src/ingest.rs"}}]}}"#;
    assert_eq!(summarize_event(read), vec!["→ Read crates/flows/src/ingest.rs"]);

    let bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git status"}}]}}"#;
    assert_eq!(summarize_event(bash), vec!["→ Bash git status"]);

    let task = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Task","input":{"subagent_type":"Explore","description":"mapear docs"}}]}}"#;
    assert_eq!(summarize_event(task), vec!["→ Task (Explore) mapear docs"]);

    // text comes with the model and multiple blocks become multiple lines
    let multi = r#"{"type":"assistant","message":{"model":"sonnet","content":[{"type":"text","text":"Achei dois RFCs"},{"type":"tool_use","name":"Grep","input":{"pattern":"RFC-","path":"docs"}}]}}"#;
    assert_eq!(
        summarize_event(multi),
        vec!["[sonnet] Achei dois RFCs", "→ Grep RFC-  in docs"]
    );
}

#[test]
fn schema_requires_tasks() {
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
fn create_stubs_materializes_backlog_normalizing_refs() {
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
    // refs normalized to uppercase
    assert_eq!(created[0].rfcs, vec!["RFC-0002"]);
    assert_eq!(created[0].task_type, TaskType::Impl);
    assert_eq!(created[1].task_type, TaskType::Spike);
    // body contains objective and criterion
    assert!(created[0].body.contains("fazer X"));
    assert!(created[0].body.contains("- [ ] criterio 1"));

    // persisted
    assert_eq!(store.list(None).unwrap().len(), 2);
}

/// Fake executor that returns a fixed `stream-json` output (with the final `result`).
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
fn run_logged_organizes_docs_by_category() {
    let dir = TmpDir::new("mirror");
    // repo with a file whose NAME looks numeric but is an ADR by content
    let repo = dir.0.join("personal-docs");
    fs::create_dir_all(&repo).unwrap();
    let doc = repo.join("0001-protocol-as-annotation.md");
    fs::write(&doc, "# ADR-0001\nprotocol as annotation\n").unwrap();
    let doc_abs = fs::canonicalize(&doc).unwrap();

    let docs_dir = dir.0.join("ext-docs");
    fs::create_dir_all(&docs_dir).unwrap();
    let store = Store::new(dir.0.join(".backlog"));

    // the agent classifies it as adr and proposes a canonical name
    let stream = format!(
        "{}\n{}",
        r#"{"type":"system","subtype":"init","model":"opus"}"#,
        serde_json::json!({
            "type":"result","subtype":"success","is_error":false,"result":"ok",
            "structured_output":{"tasks":[],"docs":[
                {"path": doc_abs.to_string_lossy(), "kind":"adr", "name":"ADR-0001-protocol-as-annotation.md"}
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
    // organized under docs_dir/adrs/<canonical-name>
    let mirrored = docs_dir.join("adrs/ADR-0001-protocol-as-annotation.md");
    assert!(mirrored.exists(), "doc was not organized in {mirrored:?}");
    assert!(
        fs::read_to_string(&mirrored).unwrap().contains("protocol as annotation")
    );
    assert!(logs.iter().any(|l| l.contains("mirrored")));
}

#[test]
fn run_logged_moves_loose_doc_in_docs_dir_to_category() {
    let dir = TmpDir::new("organize");
    let docs_dir = dir.0.join("ext-docs");
    fs::create_dir_all(&docs_dir).unwrap();
    // doc written loose at the root of docs_dir
    let loose = docs_dir.join("PRD-00-overview.md");
    fs::write(&loose, "# PRD-00\nvision\n").unwrap();
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
    // MOVED to prd/ (original removed, no duplication)
    assert!(docs_dir.join("prd/PRD-00-overview.md").exists());
    assert!(!loose.exists(), "the loose original should have been moved");
}

#[test]
fn create_stubs_dedupes_by_refs_on_rerun() {
    let dir = TmpDir::new("dedup");
    let store = Store::new(dir.0.join(".backlog"));

    create_stubs(&store, &[proposed("IR tipos", "impl", &["rfc-0002"])]).unwrap();
    // re-running with the same ref (different case) doesn't duplicate
    let again = create_stubs(&store, &[proposed("IR tipos again", "impl", &["RFC-0002"])]).unwrap();
    assert!(again.is_empty(), "should not recreate the same ref");
    assert_eq!(store.list(None).unwrap().len(), 1);
}
