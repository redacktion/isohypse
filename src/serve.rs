use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::frame::{Inbound, Outbound};
use crate::session::SessionRegistry;

const MAX_IN_FLIGHT: usize = 8;
const RESPONSE_CAP: usize = 1024;

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

async fn next_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, cap: usize) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(_) => return None,
        };
        if available.is_empty() {
            return if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&buf).into_owned())
            };
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
        let taken = available.len();
        buf.extend_from_slice(available);
        reader.consume(taken);
        if buf.len() > cap {
            return None;
        }
    }
}

pub trait Dispatcher: Send + Sync + 'static {
    fn dispatch(&self, op: &str, workspace: Option<&str>, payload: &Value) -> Result<(String, Value), String>;
}

fn error_frame(id: Option<Value>, seq: u64, message: String) -> Outbound {
    Outbound::Error { id, seq, message, retry: false, backoff_ms: None }
}

fn maybe_handle(output: String, json: Value) -> (String, Value) {
    const CAP: usize = 16 * 1024;
    if output.len() <= CAP {
        return (output, json);
    }
    match crate::objects::ObjectStore::open().and_then(|store| store.put(&output)) {
        Ok(tag) => {
            let note = format!(
                "[large output stored as handle #{tag}; {} bytes — fetch it with op \"fetch\"]\n",
                output.len()
            );
            let mut json = json;
            if let Value::Object(map) = &mut json {
                map.insert("handle".to_string(), Value::String(tag));
                map.insert("bytes".to_string(), Value::from(output.len()));
            }
            (note, json)
        }
        Err(_) => (output, json),
    }
}

pub async fn serve(listener: UnixListener, registry: Arc<SessionRegistry>, dispatcher: Arc<dyn Dispatcher>) {
    while let Ok((stream, _)) = listener.accept().await {
        let registry = Arc::clone(&registry);
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { handle_conn(stream, registry, dispatcher).await });
    }
}

async fn handle_conn(stream: UnixStream, registry: Arc<SessionRegistry>, dispatcher: Arc<dyn Dispatcher>) {
    {
        use std::os::unix::io::AsRawFd;
        if !crate::daemon::peer_ok(stream.as_raw_fd()) {
            return;
        }
    }
    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Outbound>(RESPONSE_CAP);
    let (event_tx, mut event_rx) = mpsc::channel::<Outbound>(256);
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Some(frame) => {
                        if write_half.write_all(frame.line().as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                Some(event) = event_rx.recv() => {
                    if write_half.write_all(event.line().as_bytes()).await.is_err() {
                        break;
                    }
                },
            }
        }
    });

    let mut reader = BufReader::new(read_half);
    let mut session_id: Option<String> = None;
    let cancelled: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    while let Some(line) = next_frame(&mut reader, MAX_FRAME_BYTES).await {
        if line.trim().is_empty() {
            continue;
        }
        let inbound = match Inbound::parse(&line) {
            Ok(frame) => frame,
            Err(message) => {
                let _ = tx.try_send(error_frame(None, registry.next_seq(), message));
                continue;
            }
        };
        match inbound {
            Inbound::Open { label, resume, observer } => match registry.open(&label, observer, resume) {
                Ok(ready) => {
                    if let Outbound::Ready { session, .. } = &ready {
                        session_id = Some(session.clone());
                        registry.attach_sink(session, event_tx.clone());
                    }
                    let _ = tx.try_send(ready);
                }
                Err(message) => {
                    let _ = tx.try_send(error_frame(None, registry.next_seq(), message));
                }
            },
            Inbound::Command { id, op, workspace, payload } => {
                let Some(sid) = session_id.clone() else {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), "open a session before sending commands".to_string()));
                    continue;
                };
                if crate::daemon::is_mutating_op(&op) && registry.is_observer(&sid) {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), format!("observer session cannot run mutating op {op:?}")));
                    continue;
                }
                if registry.in_flight(&sid) >= MAX_IN_FLIGHT {
                    let _ = tx.try_send(Outbound::Error { id: Some(id), seq: registry.next_seq(), message: format!("session has {MAX_IN_FLIGHT} commands in flight; retry when one finishes"), retry: true, backoff_ms: Some(50) });
                    continue;
                }
                let _ = registry.begin(&sid, &id);
                let _ = tx.try_send(Outbound::Ack { id: id.clone(), seq: registry.next_seq() });
                crate::trace::record(Some(&sid), serde_json::json!({"phase": "command", "session": &sid, "id": &id, "op": &op}));
                let registry = Arc::clone(&registry);
                let dispatcher = Arc::clone(&dispatcher);
                let cancelled = Arc::clone(&cancelled);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let dispatched = tokio::task::spawn_blocking(move || {
                        dispatcher.dispatch(&op, workspace.as_deref(), &payload)
                    })
                    .await;
                    let _ = registry.finish(&sid, &id);
                    let key = crate::session::command_key(&id);
                    let was_cancelled = cancelled.lock().map(|mut set| set.remove(&key)).unwrap_or(false);
                    if was_cancelled {
                        return;
                    }
                    let frame = match dispatched {
                        Ok(Ok((output, json))) => {
                            let (output, json) = maybe_handle(output, json);
                            Outbound::Result { id: id.clone(), seq: registry.next_seq(), output, json }
                        }
                        Ok(Err(message)) => error_frame(Some(id.clone()), registry.next_seq(), message),
                        Err(join) => error_frame(Some(id.clone()), registry.next_seq(), format!("command panicked: {join}")),
                    };
                    crate::trace::record(Some(&sid), serde_json::json!({"phase": "result", "session": &sid, "id": &id}));
                    let _ = tx.try_send(frame);
                });
            }
            Inbound::Cancel { id } => {
                if let Ok(mut set) = cancelled.lock() {
                    set.insert(crate::session::command_key(&id));
                }
                let _ = tx.try_send(Outbound::Cancelled { id, seq: registry.next_seq() });
            }
            Inbound::Subscribe { kinds, workspaces } => {
                if let Some(sid) = &session_id {
                    let _ = registry.scope(sid, kinds.clone(), workspaces.clone());
                    let _ = tx.try_send(Outbound::Snapshot {
                        seq: registry.next_seq(),
                        state: serde_json::json!({ "subscribed_kinds": kinds, "subscribed_workspaces": workspaces }),
                    });
                }
            }
            Inbound::Unsubscribe => {
                if let Some(sid) = &session_id {
                    let _ = registry.scope(sid, Vec::new(), Vec::new());
                }
            }
            Inbound::Request { body, scope } => {
                let payload = serde_json::json!({
                    "arg": body,
                    "scope": scope.unwrap_or_else(|| "session".to_string()),
                });
                let frame = match crate::daemon::session_note(&payload) {
                    Ok(output) => Outbound::Snapshot {
                        seq: registry.next_seq(),
                        state: serde_json::json!({ "request": output }),
                    },
                    Err(message) => error_frame(None, registry.next_seq(), message),
                };
                let _ = tx.try_send(frame);
            }
            Inbound::Requests => {
                let frame = match crate::daemon::session_notes() {
                    Ok(output) => Outbound::Snapshot {
                        seq: registry.next_seq(),
                        state: serde_json::json!({ "requests": output }),
                    },
                    Err(message) => error_frame(None, registry.next_seq(), message),
                };
                let _ = tx.try_send(frame);
            }
            Inbound::Reload => {
                if let Some(sid) = &session_id {
                    let cleared = registry.reset_in_flight(sid);
                    registry.attach_sink(sid, event_tx.clone());
                    let _ = tx.try_send(Outbound::Snapshot {
                        seq: registry.next_seq(),
                        state: serde_json::json!({ "reloaded": sid, "cleared_in_flight": cleared }),
                    });
                } else {
                    let _ = tx.try_send(error_frame(None, registry.next_seq(), "open a session before reload".to_string()));
                }
            }
            Inbound::Close => break,
        }
    }

    if let Some(sid) = session_id {
        registry.close(&sid);
        crate::trace::record(Some(&sid), serde_json::json!({"phase": "close", "session": &sid}));
        crate::trace::archive(&sid);
    }
    drop(tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

    struct Echo;
    impl Dispatcher for Echo {
        fn dispatch(&self, op: &str, _workspace: Option<&str>, payload: &Value) -> Result<(String, Value), String> {
            if op == "boom" {
                return Err("boom".to_string());
            }
            Ok((format!("ran {op}"), json!({ "op": op, "payload": payload })))
        }
    }

    fn temp_socket() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let unique = SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("isohypse-serve-{nanos}-{unique}.sock"));
        let _ = std::fs::remove_file(&path);
        path
    }

    async fn next_frame(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>) -> Value {
        let line = lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn open_then_command_streams_ack_and_result() {
        let path = temp_socket();
        let listener = UnixListener::bind(&path).unwrap();
        let registry = Arc::new(SessionRegistry::new());
        tokio::spawn(serve(listener, Arc::clone(&registry), Arc::new(Echo)));

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        write_half.write_all(b"{\"do\":\"session.open\",\"label\":\"codex\"}\n").await.unwrap();
        let ready = next_frame(&mut lines).await;
        assert_eq!(ready["ch"], "ready", "{ready}");
        assert!(ready["session"].as_str().unwrap().starts_with("codex."), "{ready}");

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":7,\"op\":\"context.explore\",\"payload\":{\"arg\":\"probe\"}}\n").await.unwrap();
        let ack = next_frame(&mut lines).await;
        assert_eq!(ack["ch"], "ack");
        assert_eq!(ack["id"], 7);
        let result = next_frame(&mut lines).await;
        assert_eq!(result["ch"], "result");
        assert_eq!(result["id"], 7);
        assert_eq!(result["output"], "ran context.explore");

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":8,\"op\":\"boom\"}\n").await.unwrap();
        let _ack = next_frame(&mut lines).await;
        let err = next_frame(&mut lines).await;
        assert_eq!(err["ch"], "error");
        assert_eq!(err["id"], 8);
        assert_eq!(err["message"], "boom");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn command_before_open_is_refused() {
        let path = temp_socket();
        let listener = UnixListener::bind(&path).unwrap();
        let registry = Arc::new(SessionRegistry::new());
        tokio::spawn(serve(listener, Arc::clone(&registry), Arc::new(Echo)));

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":1,\"op\":\"context.explore\"}\n").await.unwrap();
        let err = next_frame(&mut lines).await;
        assert_eq!(err["ch"], "error");
        assert!(err["message"].as_str().unwrap().contains("open a session"), "{err}");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn subscribed_session_receives_scoped_events() {
        let path = temp_socket();
        let listener = UnixListener::bind(&path).unwrap();
        let registry = Arc::new(SessionRegistry::new());
        tokio::spawn(serve(listener, Arc::clone(&registry), Arc::new(Echo)));

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        write_half.write_all(b"{\"do\":\"session.open\",\"label\":\"sub\"}\n").await.unwrap();
        let _ready = next_frame(&mut lines).await;
        write_half.write_all(b"{\"do\":\"session.subscribe\",\"kinds\":[\"file-changed\"],\"workspaces\":[]}\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snapshot = next_frame(&mut lines).await;
        assert_eq!(snapshot["ch"], "snapshot", "{snapshot}");

        registry.publish("file-changed", None, &json!({ "path": "x.rs" }));
        let event = next_frame(&mut lines).await;
        assert_eq!(event["ch"], "event");
        assert_eq!(event["kind"], "file-changed");
        assert_eq!(event["payload"]["path"], "x.rs");

        let _ = std::fs::remove_file(&path);
    }
}
