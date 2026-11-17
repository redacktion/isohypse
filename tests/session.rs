//! session.* — the frame grammar, both sockets, and multi-op transactions.
mod support;

use std::fs;
use std::sync::Arc;

use isohypse::daemon;
use isohypse::frame::{Inbound, Outbound};
use isohypse::serve::{serve, Dispatcher};
use isohypse::session::SessionRegistry;
use serde_json::{json, Value};
use support::{dispatcher, read_tag, request, scratch_dir, socket_server};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ── frame grammar ────────────────────────────────────────────────────────────

#[test]
fn an_open_frame_round_trips_with_its_resume_token() {
    let mut failed: Vec<String> = Vec::new();
    let line = r#"{"do":"session.open","label":"codex-ui-1","resume":{"session":"codex-ui-1.7f3a","token":"abc"}}"#;
    match Inbound::parse(line).unwrap() {
        Inbound::Open { label, resume, observer } => {
            { let ok: bool = (label) == ("codex-ui-1"); if !ok { failed.push("label == \"codex-ui-1\"".to_string()); } };
            { let ok: bool = !observer; if !ok { failed.push("!observer".to_string()); } };
            let resume = resume.unwrap();
            { let ok: bool = (resume.session) == ("codex-ui-1.7f3a"); if !ok { failed.push("resume.session == \"codex-ui-1.7f3a\"".to_string()); } };
            { let ok: bool = (resume.token) == ("abc"); if !ok { failed.push("resume.token == \"abc\"".to_string()); } };
        }
        other => panic!("expected open, got {other:?}"),
    }
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}

#[test]
fn a_command_frame_carries_its_id_and_payload() {
    let mut failed: Vec<String> = Vec::new();
    let line = r#"{"do":"session.command","id":7,"op":"context.explore","payload":{"arg":"multi-op"}}"#;
    match Inbound::parse(line).unwrap() {
        Inbound::Command { id, op, workspace, payload } => {
            { let ok: bool = (id) == (json!(7)); if !ok { failed.push("id == json!(7)".to_string()); } };
            { let ok: bool = (op) == ("context.explore"); if !ok { failed.push("op == \"context.explore\"".to_string()); } };
            { let ok: bool = workspace.is_none(); if !ok { failed.push("workspace.is_none()".to_string()); } };
            { let ok: bool = (payload) == (json!({"arg": "multi-op"})); if !ok { failed.push("payload == json!({{\"arg\": \"multi-op\"}})".to_string()); } };
        }
        other => panic!("expected command, got {other:?}"),
    }
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}

#[test]
fn an_error_frame_omits_false_retry_and_absent_fields() {
    let line = Outbound::Error { id: None, seq: 3, message: "boom".to_string(), retry: false, backoff_ms: None }.line();
    let __ok0: bool = line.contains("\"ch\":\"error\"");
    let __ok1: bool = !line.contains("retry");
    let __ok2: bool = !line.contains("backoff_ms");
    let __ok3: bool = !line.contains("\"id\"");

    let failed: Vec<&str> = [(__ok0, "{line}"), (__ok1, "{line}"), (__ok2, "{line}"), (__ok3, "{line}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_retry_error_frame_includes_its_backoff() {
    let line = Outbound::Error {
        id: Some(json!(2)),
        seq: 9,
        message: "rolling over".to_string(),
        retry: true,
        backoff_ms: Some(250),
    }
    .line();
    let __ok0: bool = line.contains("\"retry\":true");
    let __ok1: bool = line.contains("\"backoff_ms\":250");

    let failed: Vec<&str> = [(__ok0, "{line}"), (__ok1, "{line}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── a real session, end to end ───────────────────────────────────────────────

/// Open a session on a private socket, load the grammar, and run each command.
/// Returns one result frame per command, in order.
fn session_run(root: &std::path::Path, commands: &[(&str, Value)]) -> Vec<Value> {
    static NEXT_SOCKET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT_SOCKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let socket = std::env::temp_dir().join(format!("isohypse-session-{}-{n}.sock", std::process::id()));
    let _ = fs::remove_file(&socket);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let out = rt.block_on(async {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(serve(listener, Arc::new(SessionRegistry::new()), Arc::new(dispatcher(root))));
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (read_half, mut write) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        write.write_all(b"{\"do\":\"session.open\",\"label\":\"suite\"}\n").await.unwrap();
        assert_eq!(next_frame(&mut lines).await["ch"], "ready");
        write.write_all(b"{\"do\":\"session.command\",\"id\":0,\"op\":\"context.prompt\"}\n").await.unwrap();
        assert_eq!(next_frame(&mut lines).await["ch"], "ack");
        assert_eq!(next_frame(&mut lines).await["ch"], "result");

        let mut results = Vec::new();
        for (index, (op, payload)) in commands.iter().enumerate() {
            let id = index + 1;
            let line = format!("{}\n", json!({"do": "session.command", "id": id, "op": op, "payload": payload}));
            write.write_all(line.as_bytes()).await.unwrap();
            let ack = next_frame(&mut lines).await;
            assert_eq!(ack["ch"], "ack", "{op} did not ack: {ack}");
            let result = next_frame(&mut lines).await;
            assert_eq!(result["ch"], "result", "{op} returned {result}");
            assert_eq!(result["id"], id);
            results.push(result);
        }
        results
    });
    let _ = fs::remove_file(&socket);
    out
}

async fn next_frame(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>) -> Value {
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

fn output(frame: &Value) -> &str {
    frame["output"].as_str().unwrap()
}

#[test]
fn a_session_result_carries_no_budget_line() {
    let root = scratch_dir("no-budget");
    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();

    let frames = session_run(&root, &[("context.read", json!({"arg": "a.rs"}))]);
    let text = output(&frames[0]);
    let __ok0: bool = text.contains("fn a()");
    let __ok1: bool = !text.contains("remaining session tool calls");
    let __ok2: bool = !text.to_lowercase().contains("budget");

    let failed: Vec<&str> = [(__ok0, "the read still returns its content: {text}"), (__ok1, "the budget line is gone: {text}"), (__ok2, "no budget guidance rides the output: {text}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_session_reads_explores_edits_and_reindexes() {
    let root = scratch_dir("session-edit");
    fs::write(root.join("lib.rs"), "fn greet() -> String {\n    String::from(\"hi\")\n}\n\nfn main() {\n    greet();\n}\n").unwrap();
    let mut patcher = isohypse::patcher::Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "lib.rs");

    let frames = session_run(&root, &[
        ("context.explore", json!({"arg": "main", "to": "greet"})),
        ("mutate.edit", json!({"path": "lib.rs", "tag": tag, "validate": "none",
            "edits": [{"replace": {"lines": [2, 2]}, "with": "    String::from(\"hello\")"}]})),
        ("state.up", json!({"reindex": true})),
    ]);
    let __ok0: bool = output(&frames[0]).contains("main -> greet") || output(&frames[0]).contains("main -(");
    let __ok1: bool = output(&frames[1]).contains("updated [lib.rs#");
    let __ok2: bool = fs::read_to_string(root.join("lib.rs")).unwrap().contains("hello");
    let __ok3: bool = output(&frames[2]).contains("reindex started");

    let failed: Vec<&str> = [(__ok0, "{}"), (__ok1, "{}"), (__ok2, "fs::read_to_string(root.join(\"lib.rs\")).unwrap().contains(\"hello\")"), (__ok3, "{}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_session_stores_and_fetches_a_payload_by_tag() {
    let root = scratch_dir("session-store");
    fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();

    let frames = session_run(&root, &[
        ("state.put", json!({"arg": "handle payload"})),
        ("state.get", json!({"arg": isohypse::tag::display_tag("handle payload")})),
    ]);
    let __ok0: bool = output(&frames[0]).contains('#');
    let __ok1: bool = (output(&frames[1]).lines().next().unwrap()) == ("handle payload");

    let failed: Vec<&str> = [(__ok0, "{}"), (__ok1, "{}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_session_reports_parse_errors_with_the_path() {
    let root = scratch_dir("session-diagnose");
    fs::write(root.join("broken.rs"), "fn a() {\n    let x = \n}\n}\n").unwrap();

    let frames = session_run(&root, &[("verify.diagnose", json!({"arg": "broken.rs"}))]);
    assert!(output(&frames[0]).contains("broken.rs:"), "{}", output(&frames[0]));
}

#[test]
fn a_saved_macro_runs_its_steps() {
    let root = scratch_dir("session-macro");
    fs::write(root.join("m.rs"), "fn m() {}\n").unwrap();
    let name = format!("suite-macro-{}", std::process::id());

    let frames = session_run(&root, &[
        ("macro.save", json!({"name": name, "steps": [{"op": "state.status"}]})),
        ("macro.run", json!({"name": name})),
    ]);
    let __ok0: bool = output(&frames[0]).contains("saved");
    let __ok1: bool = output(&frames[1]).contains("1 step(s)");

    if let Ok(store) = std::env::var("ISOHYPSE_STORE") {
        let _ = fs::remove_file(std::path::PathBuf::from(store).join("macros").join(format!("{name}.json")));
    }

    let failed: Vec<&str> = [(__ok0, "{}"), (__ok1, "{}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── the workspace socket ─────────────────────────────────────────────────────

#[test]
fn the_workspace_socket_serves_status_and_up() {
    let root = scratch_dir("workspace-socket");
    fs::write(root.join("lib.rs"), "fn greet() {}\n").unwrap();
    let handle = socket_server(&root, 2);

    let status = request(&root, &json!({"op": "state.status"}));
    let __ok0: bool = status.contains("indexed files: 1");
    let __ok1: bool = (request(&root, &json!({"op": "state.up", "reindex": true}))) == ("reindex started\n");

    handle.join().unwrap();

    let failed: Vec<&str> = [(__ok0, "{status}"), (__ok1, "request(&root, &json!({{\"op\": \"state.up\", \"reindex\": true}})) == \"reinde")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn the_workspace_socket_refuses_an_op_that_needs_a_session() {
    let root = scratch_dir("workspace-socket-gate");
    fs::write(root.join("lib.rs"), "fn greet() {}\n").unwrap();
    let handle = socket_server(&root, 1);
    let socket = daemon::find_socket(&root).unwrap();

    let refused = daemon::request(&socket, &json!({"op": "context.explore", "arg": "greet"})).unwrap_err();
    let __ok0: bool = refused.contains("not available on the workspace socket");
    let __ok1: bool = refused.contains("session");

    handle.join().unwrap();

    let failed: Vec<&str> = [(__ok0, "{refused}"), (__ok1, "the refusal names the way in: {refused}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── multi-op transactions ────────────────────────────────────────────────────

#[test]
fn a_multi_op_applies_every_step_and_keeps_the_mutation_ledger() {
    let root = scratch_dir("multi-op");
    fs::write(root.join("a.rs"), "fn a() -> u8 {\n    1\n}\n").unwrap();
    fs::write(root.join("b.rs"), "fn b() -> u8 {\n    2\n}\n").unwrap();
    let mut patcher = isohypse::patcher::Patcher::new(&root).unwrap();
    let tag_a = read_tag(&mut patcher, "a.rs");
    let tag_b = read_tag(&mut patcher, "b.rs");

    let (summary, report) = dispatcher(&root)
        .dispatch(
            "multi-op",
            None,
            &json!({"op": "multi-op", "validate": "none", "steps": [
                {"op": "mutate.edit", "path": "a.rs", "tag": tag_a,
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    11"}]},
                {"op": "mutate.edit", "path": "b.rs", "tag": tag_b,
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    22"}]},
                {"op": "mutate.create", "path": "c.txt", "content": "\u{feff}created\r\n"}
            ]}),
        )
        .unwrap();
    let __ok0: bool = summary.contains("3 step(s), transactional");
    let __ok1: bool = report["mutate"][0]["json"]["sections"][0]["preview"].as_str().unwrap().contains("+    11");
    let __ok2: bool = (report["mutate"][2]["json"]["content"]) == ("created\n");
    let __ok3: bool = fs::read_to_string(root.join("a.rs")).unwrap().contains("11");
    let __ok4: bool = fs::read_to_string(root.join("b.rs")).unwrap().contains("22");

    let failed: Vec<&str> = [(__ok0, "{summary}"), (__ok1, "report[\"mutate\"][0][\"json\"][\"sections\"][0][\"preview\"].as_str().unwrap("), (__ok2, "report[\"mutate\"][2][\"json\"][\"content\"] == \"created\\n\""), (__ok3, "fs::read_to_string(root.join(\"a.rs\")).unwrap().contains(\"11\")"), (__ok4, "fs::read_to_string(root.join(\"b.rs\")).unwrap().contains(\"22\")")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_failing_step_reverts_every_prior_step() {
    let root = scratch_dir("multi-op-rollback");
    fs::write(root.join("a.rs"), "fn a() -> u8 {\n    1\n}\n").unwrap();
    fs::write(root.join("b.rs"), "fn b() -> u8 {\n    2\n}\n").unwrap();
    let mut patcher = isohypse::patcher::Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "a.rs");

    let error = dispatcher(&root)
        .dispatch(
            "multi-op",
            None,
            &json!({"op": "multi-op", "validate": "none", "steps": [
                {"op": "mutate.edit", "path": "a.rs", "tag": tag,
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]},
                {"op": "mutate.edit", "path": "b.rs", "tag": "deadbeefdeadbeef",
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]}
            ]}),
        )
        .unwrap_err();
    let __ok0: bool = error.contains("reverted");
    let __ok1: bool = !fs::read_to_string(root.join("a.rs")).unwrap().contains("99");

    let failed: Vec<&str> = [(__ok0, "rollback message: {error}"), (__ok1, "step 1 rolled back")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_validation_failure_reverts_the_edit() {
    let root = scratch_dir("multi-op-validate");
    fs::write(root.join("a.rs"), "fn a() -> u8 {\n    1\n}\n").unwrap();
    let mut patcher = isohypse::patcher::Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "a.rs");

    let error = dispatcher(&root)
        .dispatch(
            "multi-op",
            None,
            &json!({"op": "multi-op", "validate": "exit 1", "steps": [
                {"op": "mutate.edit", "path": "a.rs", "tag": tag,
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    77"}]}
            ]}),
        )
        .unwrap_err();
    let __ok0: bool = error.contains("verify");
    let __ok1: bool = !fs::read_to_string(root.join("a.rs")).unwrap().contains("77");

    let failed: Vec<&str> = [(__ok0, "verify failure surfaced: {error}"), (__ok1, "verify failure rolled back")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_multi_op_delete_step_carries_the_removed_text() {
    let root = scratch_dir("delete-diff");
    fs::write(root.join("gone.txt"), "line one\nline two\n").unwrap();

    let (_, report) = dispatcher(&root)
        .dispatch("multi-op", None, &json!({"op": "multi-op", "validate": "none", "steps": [
            {"op": "mutate.delete", "path": "gone.txt"}
        ]}))
        .unwrap();
    let step = &report["mutate"][0]["json"];
    let __ok0: bool = (step["op"]) == ("mutate.delete");
    let __ok1: bool = (step["path"]) == ("gone.txt");
    let __ok2: bool = (step["content"]) == ("line one\nline two\n");
    let __ok3: bool = !root.join("gone.txt").exists();

    let failed: Vec<&str> = [(__ok0, "step[\"op\"] == \"mutate.delete\""), (__ok1, "step[\"path\"] == \"gone.txt\""), (__ok2, "the host renders the deletion from this text"), (__ok3, "!root.join(\"gone.txt\").exists()")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_multi_op_reads_saved_history_without_leaving_its_scope() {
    let mut failed: Vec<String> = Vec::new();
    let root = scratch_dir("multi-op-history");
    let archived = root.with_extension("archived");
    let store = isohypse::objects::ObjectStore::open().unwrap();
    let full = store.put("fn retained_native_history() {}\n").unwrap();
    let path = archived.join("old.rs");
    store.journal_record(&path.to_string_lossy(), &full).unwrap();

    let (output, _) = dispatcher(&root)
        .dispatch(
            "multi-op",
            None,
            &json!({"steps": [
                {"op": "context.log", "workspace": archived, "tag": full},
                {"op": "context.read", "workspace": archived, "tag": full}
            ]}),
        )
        .unwrap();
    { let ok: bool = output.contains("retained_native_history"); if !ok { failed.push("{output}".to_string()); } };
    { let ok: bool = !path.exists(); if !ok { failed.push("retrieval must not restore source".to_string()); } };

    for step in [
        json!({"op": "context.read", "workspace": archived, "arg": "old.rs"}),
        json!({"op": "mutate.create", "workspace": archived, "path": "old.rs", "content": "bad"}),
    ] {
        let error = dispatcher(&root)
            .dispatch("multi-op", None, &json!({"steps": [step]}))
            .unwrap_err();
        { let ok: bool = error.contains("cannot target other workspaces"); if !ok { failed.push("{error}".to_string()); } };
    }
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}
