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
    let line = r#"{"do":"session.open","label":"codex-ui-1","resume":{"session":"codex-ui-1.7f3a","token":"abc"}}"#;
    match Inbound::parse(line).unwrap() {
        Inbound::Open { label, resume, observer } => {
            assert_eq!(label, "codex-ui-1");
            assert!(!observer);
            let resume = resume.unwrap();
            assert_eq!(resume.session, "codex-ui-1.7f3a");
            assert_eq!(resume.token, "abc");
        }
        other => panic!("expected open, got {other:?}"),
    }
}

#[test]
fn a_command_frame_carries_its_id_and_payload() {
    let line = r#"{"do":"session.command","id":7,"op":"context.explore","payload":{"arg":"multi-op"}}"#;
    match Inbound::parse(line).unwrap() {
        Inbound::Command { id, op, workspace, payload } => {
            assert_eq!(id, json!(7));
            assert_eq!(op, "context.explore");
            assert!(workspace.is_none());
            assert_eq!(payload, json!({"arg": "multi-op"}));
        }
        other => panic!("expected command, got {other:?}"),
    }
}

#[test]
fn an_error_frame_omits_false_retry_and_absent_fields() {
    let line = Outbound::Error { id: None, seq: 3, message: "boom".to_string(), retry: false, backoff_ms: None }.line();
    assert!(line.contains("\"ch\":\"error\""), "{line}");
    assert!(!line.contains("retry"), "{line}");
    assert!(!line.contains("backoff_ms"), "{line}");
    assert!(!line.contains("\"id\""), "{line}");
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
    assert!(line.contains("\"retry\":true"), "{line}");
    assert!(line.contains("\"backoff_ms\":250"), "{line}");
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
    assert!(text.contains("fn a()"), "the read still returns its content: {text}");
    assert!(!text.contains("remaining session tool calls"), "the budget line is gone: {text}");
    assert!(!text.to_lowercase().contains("budget"), "no budget guidance rides the output: {text}");
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
    assert!(output(&frames[0]).contains("main -> greet") || output(&frames[0]).contains("main -("), "{}", output(&frames[0]));
    assert!(output(&frames[1]).contains("updated [lib.rs#"), "{}", output(&frames[1]));
    assert!(fs::read_to_string(root.join("lib.rs")).unwrap().contains("hello"));
    assert!(output(&frames[2]).contains("reindex started"), "{}", output(&frames[2]));
}

#[test]
fn a_session_stores_and_fetches_a_payload_by_tag() {
    let root = scratch_dir("session-store");
    fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();

    let frames = session_run(&root, &[
        ("state.put", json!({"arg": "handle payload"})),
        ("state.get", json!({"arg": isohypse::tag::display_tag("handle payload")})),
    ]);
    assert!(output(&frames[0]).contains('#'), "{}", output(&frames[0]));
    assert_eq!(output(&frames[1]).lines().next().unwrap(), "handle payload", "{}", output(&frames[1]));
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
    assert!(output(&frames[0]).contains("saved"), "{}", output(&frames[0]));
    assert!(output(&frames[1]).contains("1 step(s)"), "{}", output(&frames[1]));

    if let Ok(store) = std::env::var("ISOHYPSE_STORE") {
        let _ = fs::remove_file(std::path::PathBuf::from(store).join("macros").join(format!("{name}.json")));
    }
}

// ── the workspace socket ─────────────────────────────────────────────────────

#[test]
fn the_workspace_socket_serves_status_and_up() {
    let root = scratch_dir("workspace-socket");
    fs::write(root.join("lib.rs"), "fn greet() {}\n").unwrap();
    let handle = socket_server(&root, 2);

    let status = request(&root, &json!({"op": "state.status"}));
    assert!(status.contains("indexed files: 1"), "{status}");
    assert_eq!(request(&root, &json!({"op": "state.up", "reindex": true})), "reindex started\n");

    handle.join().unwrap();
}

#[test]
fn the_workspace_socket_refuses_an_op_that_needs_a_session() {
    let root = scratch_dir("workspace-socket-gate");
    fs::write(root.join("lib.rs"), "fn greet() {}\n").unwrap();
    let handle = socket_server(&root, 1);
    let socket = daemon::find_socket(&root).unwrap();

    let refused = daemon::request(&socket, &json!({"op": "context.explore", "arg": "greet"})).unwrap_err();
    assert!(refused.contains("not available on the workspace socket"), "{refused}");
    assert!(refused.contains("session"), "the refusal names the way in: {refused}");

    handle.join().unwrap();
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
    assert!(summary.contains("3 step(s), transactional"), "{summary}");
    assert!(report["mutate"][0]["json"]["sections"][0]["preview"].as_str().unwrap().contains("+    11"));
    assert_eq!(report["mutate"][2]["json"]["content"], "created\n");
    assert!(fs::read_to_string(root.join("a.rs")).unwrap().contains("11"));
    assert!(fs::read_to_string(root.join("b.rs")).unwrap().contains("22"));
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
    assert!(error.contains("reverted"), "rollback message: {error}");
    assert!(!fs::read_to_string(root.join("a.rs")).unwrap().contains("99"), "step 1 rolled back");
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
    assert!(error.contains("verify"), "verify failure surfaced: {error}");
    assert!(!fs::read_to_string(root.join("a.rs")).unwrap().contains("77"), "verify failure rolled back");
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
    assert_eq!(step["op"], "mutate.delete");
    assert_eq!(step["path"], "gone.txt");
    assert_eq!(step["content"], "line one\nline two\n", "the host renders the deletion from this text");
    assert!(!root.join("gone.txt").exists());
}

#[test]
fn a_multi_op_reads_saved_history_without_leaving_its_scope() {
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
    assert!(output.contains("retained_native_history"), "{output}");
    assert!(!path.exists(), "retrieval must not restore source");

    for step in [
        json!({"op": "context.read", "workspace": archived, "arg": "old.rs"}),
        json!({"op": "mutate.create", "workspace": archived, "path": "old.rs", "content": "bad"}),
    ] {
        let error = dispatcher(&root)
            .dispatch("multi-op", None, &json!({"steps": [step]}))
            .unwrap_err();
        assert!(error.contains("cannot target other workspaces"), "{error}");
    }
}
