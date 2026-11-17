use super::*;
use pretty_assertions::assert_eq;

fn fixture(script: &str) -> (tempfile::TempDir, Arc<Runtime>) {
    let dir = tempfile::tempdir().unwrap();
    let worker = dir.path().join("worker.sh");
    std::fs::write(&worker, script).unwrap();
    let runtime = Arc::new(Runtime {
        config: NativeConfig {
            node: PathBuf::from("/bin/sh"),
            worker,
            adapter_config: dir.path().join("unused"),
            schema: dir.path().join("unused"),
        },
        spec: ToolSpec::Function(ResponsesApiTool {
            name: "isohypse".into(),
            description: "fixture".into(),
            strict: false,
            defer_loading: None,
            output_schema: None,
            parameters: serde_json::from_value(json!({"type":"object"})).unwrap(),
        }),
        workspace: dir.path().to_path_buf(),
        thread_id: "fixture".into(),
        state: Mutex::new(State::default()),
    });
    (dir, runtime)
}

#[tokio::test]
async fn idle_turn_abort_preserves_session_until_thread_close() {
    let (_dir, runtime) = fixture(
        "n=0\nwhile IFS= read -r line; do\ncase \"$line\" in *close*) exit 0;; esac\nn=$((n+1))\nprintf '{\"success\":true,\"sequence\":%s}\\n' \"$n\"\ndone\n",
    );
    runtime.abort().await;
    assert_eq!(
        runtime.run(json!({"callId":"one"})).await.unwrap(),
        json!({"success":true,"sequence":1})
    );
    runtime.abort().await;
    assert_eq!(
        runtime.run(json!({"callId":"two"})).await.unwrap(),
        json!({"success":true,"sequence":2})
    );
    runtime.close().await;
    assert!(
        runtime
            .run(json!({"callId":"three"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("no replay")
    );
}

#[tokio::test]
async fn cancelled_native_call_cannot_consume_a_late_response() {
    let (dir, runtime) = fixture(
        "while IFS= read -r line; do\ncase \"$line\" in *close*) exit 0;; esac\ntouch started\nsleep 0.2\nprintf '{\"success\":true}\\n'\ndone\n",
    );
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move { task_runtime.run(json!({"callId":"cancelled"})).await });
    for _ in 0..100 {
        if dir.path().join("started").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(dir.path().join("started").exists());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    runtime.abort().await;
    assert!(
        runtime
            .run(json!({"callId":"next"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("no replay")
    );
    let reconnected = runtime
        .run(json!({"callId":"reconnect", "arguments":{"steps":[{"op":"session.reconnect"}]}}))
        .await
        .unwrap();
    assert_eq!(reconnected["success"], true);
    assert!(
        reconnected["output"]
            .as_str()
            .unwrap()
            .contains("unconfirmed")
    );
    assert!(
        runtime
            .run(json!({"callId":"cancelled"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("Duplicate")
    );
    assert_eq!(
        runtime.run(json!({"callId":"fresh"})).await.unwrap()["success"],
        true
    );
    runtime.close().await;
    assert!(runtime.run(json!({"callId":"closed-reconnect", "arguments":{"steps":[{"op":"session.reconnect"}]}})).await.unwrap_err().to_string().contains("thread closed"));
}

#[derive(Default)]
struct FileItems(std::sync::Mutex<Vec<FileChangeItem>>);

impl codex_tools::TurnItemEmitter for FileItems {
    fn emit_started<'a>(
        &'a self,
        _item: codex_tools::ExtensionTurnItem,
    ) -> codex_tools::TurnItemEmissionFuture<'a> {
        panic!("Expected canonical file changes");
    }
    fn emit_completed<'a>(
        &'a self,
        _item: codex_tools::ExtensionTurnItem,
    ) -> codex_tools::TurnItemEmissionFuture<'a> {
        panic!("Expected canonical file changes");
    }
    fn emit_file_change<'a>(
        &'a self,
        item: FileChangeItem,
    ) -> codex_tools::TurnItemEmissionFuture<'a> {
        self.0.lock().unwrap().push(item);
        Box::pin(std::future::ready(()))
    }
}

#[tokio::test]
async fn confirmed_mutations_emit_exact_diffs_but_failures_do_not() {
    let (_dir, runtime) = fixture("");
    let emitter = FileItems::default();
    let diff = "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let mutations = json!([
        {"op":"mutate.create","json":{"path":"a.txt","content":"one\n"}},
        {"op":"mutate.edit","json":{"sections":[{"path":"a.txt","preview":diff}]}}
    ]);
    let mut result = json!({"success":true,"output":"confirmed","mutations":mutations});
    runtime.publish_changes("call", &emitter, &mut result).await;
    assert_eq!(result, json!({"success":true,"output":"confirmed"}));
    {
        let items = emitter.0.lock().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].status, None);
        assert_eq!(items[1].status, Some(PatchApplyStatus::Completed));
        assert_eq!(items[0].id, items[1].id);
        assert_ne!(items[1].id, items[3].id);
        assert_eq!(
            items[1].changes[&runtime.workspace.join("a.txt")],
            FileChange::Add {
                content: "one\n".into()
            }
        );
        assert_eq!(
            items[3].changes[&runtime.workspace.join("a.txt")],
            FileChange::Update {
                unified_diff: diff.into(),
                move_path: None
            }
        );
    }
    let mut failed =
        json!({"success":false,"output":"validation failed; reverted","mutations":mutations});
    runtime
        .publish_changes("failed", &emitter, &mut failed)
        .await;
    assert_eq!(emitter.0.lock().unwrap().len(), 4);
    assert!(failed.get("mutations").is_none());
    let mut older_daemon = json!({"success":true,"output":"created","mutations":[{"op":"mutate.create","json":{"path":"b.txt"}}]});
    runtime
        .publish_changes("old", &emitter, &mut older_daemon)
        .await;
    assert_eq!(emitter.0.lock().unwrap().len(), 4);
    assert!(
        older_daemon["output"]
            .as_str()
            .unwrap()
            .contains("structured diff unavailable")
    );
}
