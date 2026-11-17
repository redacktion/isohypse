//! `isohypse call` parsing, framing, and outcome handling. Pure except the
//! socket round trip at the end.

mod support;

use std::fs;
use std::sync::Arc;

use isohypse::call::{frames, outcome, Batch, CallError, MAX_INPUT_BYTES};
use isohypse::serve::serve;
use isohypse::session::SessionRegistry;
use serde_json::{json, Value};
use support::{dispatcher, scratch_dir};

fn parses(input: &str) -> bool {
    Batch::parse(input).is_ok()
}

#[test]
fn batch_parse_accepts_the_documented_shapes() {
    let got: Vec<bool> = [
        r#"{"steps":[{"op":"context.read","arg":"a"}]}"#,
        r#"{"steps":[{"op":"mutate.create","path":"a"}],"validate":"true"}"#,
        r#"{"op":"context.read","arg":"a"}"#,
        r#"[{"op":"context.read","arg":"a"}]"#,
    ]
    .iter()
    .map(|i| parses(i))
    .collect();
    assert_eq!(got, vec![true; 4]);
}

#[test]
fn batch_parse_rejects_malformed_input() {
    let got: Vec<bool> = ["{nope", "[1]", "3", r#"{"validate":"true"}"#, r#"{"steps":[]}"#, r#"{"steps":"x"}"#, r#"{"steps":[{"arg":"a"}]}"#, r#"{"steps":[{"op":""}]}"#, r#"{"steps":[{"op":"a"}],"extra":1}"#]
        .iter()
        .map(|i| parses(i))
        .collect();
    assert_eq!(got, vec![false; 9]);
}

#[test]
fn batch_parse_errors_are_typed() {
    let got: Vec<CallError> = ["{nope", r#"{"steps":[]}"#, r#"{"steps":[{"arg":"a"}]}"#, r#"{"steps":[{"op":"a"}],"extra":1}"#]
        .iter()
        .map(|i| Batch::parse(i).unwrap_err())
        .collect();
    let want = vec![
        CallError::InvalidJson("key must be a string at line 1 column 2".to_string()),
        CallError::StepCount(0),
        CallError::MissingOp(1),
        CallError::UnknownField("extra".to_string()),
    ];
    assert_eq!(got, want);
}

#[test]
fn batch_parse_refuses_more_than_32_steps() {
    let steps = vec![json!({"op": "context.read", "arg": "a"}); 33];
    assert_eq!(Batch::parse(&json!({"steps": steps}).to_string()).unwrap_err(), CallError::StepCount(33));
}

#[test]
fn batch_parse_refuses_input_over_the_byte_limit() {
    let big = format!(r#"{{"op":"context.read","arg":"{}"}}"#, "a".repeat(MAX_INPUT_BYTES));
    assert!(matches!(Batch::parse(&big), Err(CallError::TooLarge(_))));
}

#[test]
fn batch_parse_error_text_shows_the_expected_shape() {
    let text = Batch::parse("{\"steps\": [oops]").unwrap_err().to_string();
    assert!(text.contains("expected {\"steps\""));
}

#[test]
fn text_fields_given_as_arrays_join_with_newlines() {
    let batch = Batch::parse(r#"{"steps":[{"op":"mutate.edit","path":"b.rs","tag":"abc1234","edits":[{"replace":{"lines":[1,1]},"with":["x","y"]}]}],"validate":"none"}"#).unwrap();
    let frame = &frames(&batch)[2];
    let sent: Value = serde_json::from_str(frame["bind"]["batch"].as_str().unwrap()).unwrap();
    assert_eq!(sent["steps"][0]["edits"][0]["with"], json!("x\ny"));
}

#[test]
fn content_arrays_end_with_a_newline() {
    let batch = Batch::parse(r#"{"op":"mutate.create","path":"a.rs","content":["fn a() {","}"]}"#).unwrap();
    assert_eq!(frames(&batch)[2]["payload"]["content"], json!("fn a() {\n}\n"));
}

#[test]
fn a_single_step_batch_runs_as_one_command() {
    let batch = Batch::parse(r#"{"steps":[{"op":"context.read","arg":"a.rs"}]}"#).unwrap();
    let kinds: Vec<String> = frames(&batch).iter().map(|f| f["do"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, vec!["session.open", "session.command", "session.command", "session.close"]);
}

#[test]
fn a_two_step_batch_runs_as_a_multiop() {
    let batch = Batch::parse(r#"{"steps":[{"op":"context.read","arg":"a"},{"op":"context.find","arg":"b"}]}"#).unwrap();
    let kinds: Vec<String> = frames(&batch).iter().map(|f| f["do"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, vec!["session.open", "session.command", "session.multiop", "session.close"]);
}

#[test]
fn a_single_step_with_validate_runs_as_a_multiop() {
    let batch = Batch::parse(r#"{"steps":[{"op":"mutate.create","path":"a"}],"validate":"true"}"#).unwrap();
    assert_eq!(frames(&batch)[2]["do"], json!("session.multiop"));
}

#[test]
fn every_call_loads_the_grammar_first() {
    let batch = Batch::parse(r#"{"op":"context.read","arg":"a"}"#).unwrap();
    assert_eq!(frames(&batch)[1]["op"], json!("context.prompt"));
}

fn result(id: u64, output: &str) -> String {
    json!({"ch": "result", "id": id, "output": output}).to_string()
}

fn error(id: u64, message: &str) -> String {
    json!({"ch": "error", "id": id, "message": message}).to_string()
}

#[test]
fn outcome_returns_the_batch_output() {
    let ack = json!({"ch": "ack", "id": 1}).to_string();
    assert_eq!(outcome(vec![result(0, "grammar"), ack, result(1, "body")].into_iter()), Ok("body".to_string()));
}

#[test]
fn outcome_formats_a_structured_failure() {
    let failure = json!({"kind": "isohypse_failure", "operation": "mutate.edit", "path": "a.rs", "current": "stale tag"}).to_string();
    assert_eq!(outcome(vec![result(0, "g"), error(1, &failure)].into_iter()), Err(CallError::Failure("op mutate.edit, path a.rs: stale tag".to_string())));
}

#[test]
fn outcome_passes_a_plain_error_through() {
    assert_eq!(outcome(vec![error(0, "plain")].into_iter()), Err(CallError::Failure("plain".to_string())));
}

#[test]
fn outcome_explains_a_prompt_denied_by_policy() {
    let text = outcome(vec![error(0, "enforce: op \"context.prompt\" is not in the workspace allowlist")].into_iter()).unwrap_err().to_string();
    assert!(text.contains("must allow context.prompt"));
}

#[test]
fn outcome_without_the_batch_result_is_unconfirmed() {
    assert_eq!(outcome(vec![result(0, "grammar")].into_iter()), Err(CallError::Unconfirmed));
}

#[test]
fn outcome_skips_unparseable_lines() {
    assert_eq!(outcome(vec!["garbage".to_string(), result(1, "ok")].into_iter()), Ok("ok".to_string()));
}

fn round_trip(input: &'static str) -> Result<String, CallError> {
    let root = scratch_dir(&format!("call-run-{}", input.len()));
    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let socket = std::env::temp_dir().join(format!("isohypse-call-{}-{}.sock", std::process::id(), input.len()));
    let _ = fs::remove_file(&socket);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let out = rt.block_on(async {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(serve(listener, Arc::new(SessionRegistry::new()), Arc::new(dispatcher(&root))));
        let path = socket.clone();
        tokio::task::spawn_blocking(move || isohypse::call::run(&path, input)).await.unwrap()
    });
    let _ = fs::remove_file(&socket);
    out
}

#[test]
fn run_reads_a_file_through_a_session_socket() {
    assert!(round_trip(r#"{"steps":[{"op":"context.read","arg":"a.rs"}]}"#).unwrap().contains("1:fn a() {}"));
}

#[test]
fn run_labels_each_step_of_a_multiop() {
    let text = round_trip(r#"{"steps":[{"op":"context.read","arg":"a.rs","label":"first"},{"op":"context.find","arg":"fn a"}]}"#).unwrap();
    assert!(text.contains("-- step 1 (context.read): first"));
}
