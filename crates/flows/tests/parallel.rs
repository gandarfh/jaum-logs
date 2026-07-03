use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor, Session};
use jaum_core::Store;
use jaum_flows::parallel::{
    Parallel, ParallelReport, build_prompt, parse_stream, parse_structured, schema,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-parallel-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Recording executor with a fixed `stream-json` output (the default
/// `spawn_oneshot_streaming` feeds the lines from `spawn_oneshot`).
struct RecStream {
    out: String,
    calls: RefCell<Vec<(String, ExecFlags)>>,
}
impl Executor for RecStream {
    fn spawn_oneshot(&self, p: &str, f: &ExecFlags) -> anyhow::Result<String> {
        self.calls.borrow_mut().push((p.to_string(), f.clone()));
        Ok(self.out.clone())
    }
    fn spawn_interactive(&self, _p: &str, _f: &ExecFlags) -> anyhow::Result<Session> {
        ClaudeExecutor::with_bin("cat").spawn_interactive("", &ExecFlags::default())
    }
}

const OPEN_TASK: &str = r#"---
id: TASK-001
type: impl
status: ready
prs:
  - repo: org/slyde
    pr: 0
    branch: feat/render
---

## Objective
Render engine
"#;

const OPEN_TASK_NO_REPO: &str = r#"---
id: TASK-002
type: impl
status: backlog
---
"#;

const MERGED_TASK: &str = r#"---
id: TASK-003
type: impl
status: merged
---

## Objective
done already
"#;

fn store_with(dir: &TmpDir, fixtures: &[(&str, &str)]) -> Store {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for (id, body) in fixtures {
        fs::write(backlog.join(format!("{id}.md")), body).unwrap();
    }
    Store::new(&backlog)
}

/// Envelope as `claude --output-format json --json-schema` returns it.
const ENVELOPE: &str = r#"{
  "type":"result","subtype":"success","is_error":false,
  "result":"ok",
  "structured_output":{
    "conflicts":[
      {"a":"TASK-002","b":"TASK-009","repo":"org/slyde","reason":"both edit src/render.rs"}
    ]
  }
}"#;

#[test]
fn parse_structured_extracts_conflicts() {
    let r = parse_structured(ENVELOPE).unwrap();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].a, "TASK-002");
    assert_eq!(r.conflicts[0].b, "TASK-009");
    assert_eq!(r.conflicts[0].repo, "org/slyde");
    assert!(r.conflicts[0].reason.contains("render.rs"));
}

#[test]
fn parse_stream_takes_last_result() {
    let stream = format!(
        "{}\n{}\n{}\n",
        r#"{"type":"system","subtype":"init","model":"opus"}"#,
        r#"{"type":"assistant","message":{"content":[]}}"#,
        ENVELOPE.replace('\n', "")
    );
    let r = parse_stream(&stream).unwrap();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].b, "TASK-009");
}

#[test]
fn no_conflicts_when_field_empty() {
    let env = r#"{"is_error":false,"result":"ok","structured_output":{"conflicts":[]}}"#;
    let r = parse_structured(env).unwrap();
    assert!(r.conflicts.is_empty());
}

#[test]
fn conflict_between_finds_in_either_order() {
    let r = parse_structured(ENVELOPE).unwrap();
    assert!(r.conflict_between("TASK-002", "TASK-009").is_some());
    // reversed order also finds it
    assert!(r.conflict_between("TASK-009", "TASK-002").is_some());
    assert!(r.conflict_between("TASK-002", "TASK-100").is_none());
}

#[test]
fn report_default_is_empty() {
    let r = ParallelReport::default();
    assert!(r.conflicts.is_empty());
    assert!(r.conflict_between("a", "b").is_none());
}

#[test]
fn schema_has_conflicts_required() {
    let s = schema();
    let req = s["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v == "conflicts"));
}

#[test]
fn parse_structured_propagates_is_error() {
    let env = r#"{"is_error":true,"result":"usage limit reached"}"#;
    let err = parse_structured(env).unwrap_err();
    assert!(err.to_string().contains("usage limit reached"));
}

#[test]
fn parse_structured_defaults_when_conflicts_field_missing() {
    let env = r#"{"is_error":false,"result":"ok","structured_output":{}}"#;
    let r = parse_structured(env).unwrap();
    assert!(r.conflicts.is_empty());
}

#[test]
fn parse_stream_skips_blank_and_non_json_lines() {
    let stream = format!("\n\nnot json at all\n{}\n\n", ENVELOPE.replace('\n', ""));
    let r = parse_stream(&stream).unwrap();
    assert_eq!(r.conflicts.len(), 1);
}

#[test]
fn parse_stream_without_result_event_fails() {
    let only_events = "{\"type\":\"system\",\"subtype\":\"init\"}\n";
    let err = parse_stream(only_events).unwrap_err();
    assert!(err.to_string().contains("result"));
}

#[test]
fn build_prompt_lists_repos_and_bodies() {
    let dir = TmpDir::new("prompt");
    let store = store_with(
        &dir,
        &[("TASK-001", OPEN_TASK), ("TASK-002", OPEN_TASK_NO_REPO)],
    );
    let tasks = store.list(None).unwrap();
    let p = build_prompt(&tasks);
    assert!(p.contains("TASK-001 (repos: org/slyde)"));
    assert!(p.contains("Render engine")); // task body included
    assert!(p.contains("TASK-002 (repos: none)")); // no linked repo
    assert!(p.contains("IN PARALLEL"));
}

#[test]
fn analyze_logged_skips_llm_with_fewer_than_two_open_tasks() {
    let dir = TmpDir::new("few");
    // one open + one merged: merged is filtered, so fewer than two remain.
    let store = store_with(&dir, &[("TASK-001", OPEN_TASK), ("TASK-003", MERGED_TASK)]);
    let exec = RecStream {
        out: String::new(),
        calls: RefCell::new(Vec::new()),
    };
    let par = Parallel::new(&store, &exec, dir.0.clone(), HashMap::new());

    let mut logs: Vec<String> = Vec::new();
    let r = par
        .analyze_logged(&mut |l| logs.push(l.to_string()))
        .unwrap();
    assert!(r.conflicts.is_empty());
    assert!(exec.calls.borrow().is_empty(), "must not invoke the LLM");
    assert!(logs.is_empty());
}

#[test]
fn analyze_logged_streams_prompt_flags_and_parses_report() {
    let dir = TmpDir::new("analyze");
    let store = store_with(
        &dir,
        &[
            ("TASK-001", OPEN_TASK),
            ("TASK-002", OPEN_TASK_NO_REPO),
            ("TASK-003", MERGED_TASK),
        ],
    );
    let repo_path = dir.0.join("repos/slyde");
    fs::create_dir_all(&repo_path).unwrap();
    let stream = format!(
        "{}\n{}\n",
        r#"{"type":"system","subtype":"init","model":"opus"}"#,
        ENVELOPE.replace('\n', "")
    );
    let exec = RecStream {
        out: stream,
        calls: RefCell::new(Vec::new()),
    };
    let repos = HashMap::from([("org/slyde".to_string(), repo_path.clone())]);
    let par = Parallel::new(&store, &exec, dir.0.clone(), repos);

    let mut logs: Vec<String> = Vec::new();
    let r = par
        .analyze_logged(&mut |l| logs.push(l.to_string()))
        .unwrap();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].a, "TASK-002");

    // live logs summarized from the stream events
    assert!(logs.iter().any(|l| l.contains("session started")));

    let calls = exec.calls.borrow();
    let (prompt, flags) = &calls[0];
    // prompt: only the open tasks, with repos and bodies
    assert!(prompt.contains("TASK-001 (repos: org/slyde)"));
    assert!(prompt.contains("TASK-002 (repos: none)"));
    assert!(!prompt.contains("TASK-003"), "merged task must be excluded");
    assert!(prompt.contains("Render engine"));
    // read-only flags with structured streaming output + repo access
    assert_eq!(flags.cwd.as_deref(), Some(dir.0.as_path()));
    for t in ["Edit", "Write", "NotebookEdit"] {
        assert!(flags.disallowed_tools.iter().any(|x| x == t));
    }
    assert!(flags.extra.iter().any(|x| x == "stream-json"));
    assert!(flags.extra.iter().any(|x| x == "--json-schema"));
    assert!(flags.extra.iter().any(|x| x == "--add-dir"));
    assert!(
        flags
            .extra
            .iter()
            .any(|x| x == &repo_path.to_string_lossy())
    );
}

#[test]
fn analyze_logged_without_repos_omits_add_dir() {
    let dir = TmpDir::new("nodirs");
    let store = store_with(
        &dir,
        &[("TASK-001", OPEN_TASK), ("TASK-002", OPEN_TASK_NO_REPO)],
    );
    let exec = RecStream {
        out: ENVELOPE.replace('\n', ""),
        calls: RefCell::new(Vec::new()),
    };
    let par = Parallel::new(&store, &exec, dir.0.clone(), HashMap::new());

    par.analyze_logged(&mut |_| {}).unwrap();
    let calls = exec.calls.borrow();
    let (_prompt, flags) = &calls[0];
    assert!(!flags.extra.iter().any(|x| x == "--add-dir"));
}
