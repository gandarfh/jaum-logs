use jaum_flows::parallel::{ParallelReport, parse_stream, parse_structured, schema};

/// Envelope como o `claude --output-format json --json-schema` devolve.
const ENVELOPE: &str = r#"{
  "type":"result","subtype":"success","is_error":false,
  "result":"ok",
  "structured_output":{
    "conflicts":[
      {"a":"TASK-002","b":"TASK-009","repo":"org/slyde","reason":"ambas editam src/render.rs"}
    ]
  }
}"#;

#[test]
fn parse_structured_extrai_conflitos() {
    let r = parse_structured(ENVELOPE).unwrap();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].a, "TASK-002");
    assert_eq!(r.conflicts[0].b, "TASK-009");
    assert_eq!(r.conflicts[0].repo, "org/slyde");
    assert!(r.conflicts[0].reason.contains("render.rs"));
}

#[test]
fn parse_stream_pega_ultimo_result() {
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
fn sem_conflitos_quando_campo_vazio() {
    let env = r#"{"is_error":false,"result":"ok","structured_output":{"conflicts":[]}}"#;
    let r = parse_structured(env).unwrap();
    assert!(r.conflicts.is_empty());
}

#[test]
fn conflict_between_acha_em_qualquer_ordem() {
    let r = parse_structured(ENVELOPE).unwrap();
    assert!(r.conflict_between("TASK-002", "TASK-009").is_some());
    // ordem invertida também acha
    assert!(r.conflict_between("TASK-009", "TASK-002").is_some());
    assert!(r.conflict_between("TASK-002", "TASK-100").is_none());
}

#[test]
fn report_default_e_vazio() {
    let r = ParallelReport::default();
    assert!(r.conflicts.is_empty());
    assert!(r.conflict_between("a", "b").is_none());
}

#[test]
fn schema_tem_conflicts_required() {
    let s = schema();
    let req = s["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v == "conflicts"));
}
