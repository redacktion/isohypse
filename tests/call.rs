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
fn batch_parse_rejects_malformed_input() {
    let got: Vec<bool> = ["{nope", "[1]", "3", r#"{"validate":"true"}"#, r#"{"steps":[]}"#, r#"{"steps":"x"}"#, r#"{"steps":[{"arg":"a"}]}"#, r#"{"steps":[{"op":""}]}"#, r#"{"steps":[{"op":"a"}],"extra":1}"#]
        .iter()
        .map(|i| parses(i))
        .collect();
    assert_eq!(got, vec![false; 9]);
}



#[test]
fn batch_parse_refuses_input_over_the_byte_limit() {
    let big = format!(r#"{{"op":"context.read","arg":"{}"}}"#, "a".repeat(MAX_INPUT_BYTES));
    assert!(matches!(Batch::parse(&big), Err(CallError::TooLarge(_))));
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
fn a_single_step_with_validate_runs_as_a_multiop() {
    let batch = Batch::parse(r#"{"steps":[{"op":"mutate.create","path":"a"}],"validate":"true"}"#).unwrap();
    assert_eq!(frames(&batch)[2]["do"], json!("session.multiop"));
}


fn result(id: u64, output: &str) -> String {
    json!({"ch": "result", "id": id, "output": output}).to_string()
}

fn error(id: u64, message: &str) -> String {
    json!({"ch": "error", "id": id, "message": message}).to_string()
}


#[test]
fn outcome_formats_a_structured_failure() {
    let failure = json!({"kind": "isohypse_failure", "operation": "mutate.edit", "path": "a.rs", "current": "stale tag"}).to_string();
    assert_eq!(outcome(vec![result(0, "g"), error(1, &failure)].into_iter()), Err(CallError::Failure("op mutate.edit, path a.rs: stale tag".to_string())));
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
