use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::hint::RequestKind;
use crate::frame::{Inbound, Outbound};
use crate::session::SessionRegistry;

const MAX_IN_FLIGHT: usize = 8;
const RESPONSE_CAP: usize = 1024;

const MAX_FRAME_BYTES: usize = usize::MAX;

async fn next_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, cap: usize) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(_) => return None,
        };
        if available.is_empty() {
            return if buf.is_empty() { None } else { Some(String::from_utf8(buf).unwrap_or_else(|_| "{\"error\":\"frame is not valid UTF-8\"}".to_string())) };
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > cap {
                reader.consume(pos + 1);
                return Some(format!("{{\"error\":\"frame exceeds the {} byte limit\"}}", cap));
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Some(String::from_utf8(buf).unwrap_or_else(|_| "{\"error\":\"frame is not valid UTF-8\"}".to_string()));
        }
        if buf.len() + available.len() > cap {
            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                reader.consume(pos + 1);
                return Some(format!("{{\"error\":\"frame exceeds the {} byte limit\"}}", cap));
            }
            let taken = available.len();
            reader.consume(taken);
            loop {
                let rest = match reader.fill_buf().await { Ok(bytes) => bytes, Err(_) => return None };
                if rest.is_empty() { break; }
                if let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                    reader.consume(pos + 1);
                    break;
                }
                let rest_len = rest.len();
                reader.consume(rest_len);
            }
            return Some(format!("{{\"error\":\"frame exceeds the {} byte limit\"}}", cap));
        }
        let taken = available.len();
        buf.extend_from_slice(available);
        reader.consume(taken);
    }
}

pub trait Dispatcher: Send + Sync + 'static {
    fn dispatch(&self, op: &str, workspace: Option<&str>, payload: &Value) -> Result<(String, Value), String>;
    fn dispatch_cancelable(&self, op: &str, workspace: Option<&str>, payload: &Value, _cancelled: Arc<std::sync::atomic::AtomicBool>) -> Result<(String, Value), String> {
        self.dispatch(op, workspace, payload)
    }
    fn check_enforce(&self, op: &str) -> Result<(), String> { let _ = op; Ok(()) }
    fn set_enforce(&self, request: crate::policy::EnforceRequest) -> Result<(), String> { let _ = request; Ok(()) }
}

/// The enforce frame as a typed request, plus the label echoed back to the client.
fn parse_enforce_request(enabled: bool, profile: Option<&str>) -> Result<(crate::policy::EnforceRequest, String), String> {
    use crate::policy::EnforceRequest;
    if !enabled {
        return Ok((EnforceRequest::Disable, "off".to_string()));
    }
    let name = profile.unwrap_or("readonly");
    match crate::session::resolve_profile(name)? {
        Some(policy) => Ok((EnforceRequest::Activate(policy), name.to_string())),
        None => Err(format!("unknown enforce profile {name:?}; available: {:?}", crate::session::profile_names())),
    }
}

fn check_observer(op: &str, payload: &Value) -> Result<(), String> {
    let readonly = crate::policy::Policy::builtin("readonly").unwrap_or_else(crate::policy::Policy::lockdown);
    let check = |op: &str, payload: &Value| {
        if payload.get("validate").is_some() || readonly.permits(op).is_err() {
            Err(format!("observer session cannot run op {op:?} or validation"))
        } else {
            Ok(())
        }
    };
    if op != "multi-op" { return check(op, payload); }
    if payload.get("validate").is_some() {
        return Err("observer multi-op cannot include validation".to_string());
    }
    let steps = payload.get("steps").and_then(Value::as_array)
        .filter(|steps| !steps.is_empty()).ok_or("observer multi-op needs nonempty steps")?;
    for step in steps {
        check(step.get("op").and_then(Value::as_str).unwrap_or(""), step)?;
    }
    Ok(())
}

/// The grammar is deliberately the first thing a session exposes: a client that
/// has not read context.prompt cannot know tags, anchors, or validation rules.
fn requires_schema(op: &str) -> bool {
    !matches!(op, "context.prompt" | "state.status") && !op.starts_with("session.")
}

/// A bound multi-op batch may load the grammar as one of its steps.
fn batch_loads_schema(bind: &std::collections::HashMap<String, String>) -> bool {
    let Some(raw) = bind.get("batch") else { return false };
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else { return false };
    parsed
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|step| step.get("op").and_then(Value::as_str) == Some("context.prompt"))
}


fn error_frame(id: Option<Value>, seq: u64, message: String) -> Outbound {
    let message = if serde_json::from_str::<Value>(&message).is_ok() {
        message
    } else {
        crate::opresult::failure_json("session", None, None, None, None, Some(&message), None, None, None)
    };
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
    // A client that stops draining must fail loudly: dropping its result silently
    // leaves it waiting forever on a socket that will never answer.
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Outbound>(RESPONSE_CAP);
    let (event_tx, mut event_rx) = mpsc::channel::<Outbound>(256);
    let writer = {
        let broken = Arc::clone(&broken);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = rx.recv() => match frame {
                        Some(frame) => {
                            if write_half.write_all(frame.line().as_bytes()).await.is_err() {
                                broken.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                        }
                        None => break,
                    },
                    Some(event) = event_rx.recv() => {
                        if write_half.write_all(event.line().as_bytes()).await.is_err() {
                            broken.store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        })
    };

    let mut reader = BufReader::new(read_half);
    let mut session_id: Option<String> = None;
    let mut responses = tokio::task::JoinSet::new();
    let mut graceful_close = false;
    let cancelled: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let cancellation_tokens: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    while let Some(line) = next_frame(&mut reader, MAX_FRAME_BYTES).await {
        if broken.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        while responses.try_join_next().is_some() {}
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
                if registry.is_observer(&sid) {
                    if let Err(message) = check_observer(&op, &payload) {
                        let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), message));
                        continue;
                    }
                }
                if registry.in_flight(&sid) >= MAX_IN_FLIGHT {
                    let _ = tx.try_send(Outbound::Error { id: Some(id), seq: registry.next_seq(), message: format!("session has {MAX_IN_FLIGHT} commands in flight; retry when one finishes"), retry: true, backoff_ms: Some(50) });
                    continue;
                }
                // The grammar comes first: a client that has not read it cannot
                // know tags, anchors, or the validation rules for edits.
                if op == "context.prompt" {
                    registry.mark_schema(&sid);
                } else if requires_schema(&op) && !registry.schema_loaded(&sid) {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), "call context.prompt first to load the operation grammar".to_string()));
                    continue;
                }

                if let Err(message) = dispatcher.check_enforce(&op) {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), message));
                    continue;
                }
                if let Err(message) = registry.begin(&sid, &id) {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), message));
                    continue;
                }
                let _ = tx.try_send(Outbound::Ack { id: id.clone(), seq: registry.next_seq() });

                let registry = Arc::clone(&registry);
                let dispatcher = Arc::clone(&dispatcher);
                let cancelled = Arc::clone(&cancelled);
                let active_tasks = Arc::clone(&active_tasks);
                let cancellation_tokens = Arc::clone(&cancellation_tokens);
                let tx = tx.clone();
                let key = crate::session::command_key(&id);
                let cancel_token = Arc::new(std::sync::atomic::AtomicBool::new(false));
                if let Ok(mut tokens) = cancellation_tokens.lock() { tokens.insert(key.clone(), Arc::clone(&cancel_token)); }
                let op_name = op.clone();
                let dispatched = tokio::task::spawn_blocking(move || {
                    dispatcher.dispatch_cancelable(&op, workspace.as_deref(), &payload, Arc::clone(&cancel_token))
                });
                if let Ok(mut tasks) = active_tasks.lock() {
                    tasks.insert(key.clone(), dispatched.abort_handle());
                }
                let broken = Arc::clone(&broken);

                responses.spawn(async move {
                    let dispatched = dispatched.await;
                    let _ = registry.finish(&sid, &id);
                    if let Ok(mut tasks) = active_tasks.lock() { tasks.remove(&key); }
                    if let Ok(mut tokens) = cancellation_tokens.lock() { tokens.remove(&key); }
                    let was_cancelled = cancelled.lock().map(|mut set| set.remove(&key)).unwrap_or(false);
                    if was_cancelled {
                        return;
                    }
                    let frame = match dispatched {
                        Ok(Ok((output, json))) => {
                            let (output, json) = match tokio::task::spawn_blocking(move || maybe_handle(output, json)).await {
                                Ok(pair) => pair,
                                Err(_) => return,
                            };
                            let read_only = op_name.starts_with("context.") && op_name != "context.prompt";
                            let output = if registry.note_request(&sid, RequestKind::of(1, read_only)) { format!("{output}\n{}\n", crate::hint::TEXT) } else { output };
                            Outbound::Result { id: id.clone(), seq: registry.next_seq(), output, json }
                        }
                        Ok(Err(message)) => error_frame(Some(id.clone()), registry.next_seq(), message),
                        Err(join) => error_frame(Some(id.clone()), registry.next_seq(), format!("command panicked: {join}")),
                    };
                    crate::trace::record(Some(&sid), serde_json::json!({"phase": "result", "session": &sid, "id": &id}));
                    if tx.try_send(frame).is_err() {
                        broken.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
            Inbound::MultiOp { id, expr, bind, workspace } => {
                let Some(sid) = session_id.clone() else {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), "open a session before sending commands".to_string()));
                    continue;
                };
                if registry.in_flight(&sid) >= MAX_IN_FLIGHT {
                    let _ = tx.try_send(Outbound::Error { id: Some(id), seq: registry.next_seq(), message: format!("session has {MAX_IN_FLIGHT} commands in flight; retry when one finishes"), retry: true, backoff_ms: Some(50) });
                    continue;
                }
                if batch_loads_schema(&bind) {
                    registry.mark_schema(&sid);
                } else if !registry.schema_loaded(&sid) {
                    let _ = tx.try_send(error_frame(Some(id), registry.next_seq(), "call context.prompt first to load the operation grammar".to_string()));
                    continue;
                }

                let _ = registry.begin(&sid, &id);
                let _ = tx.try_send(Outbound::Ack { id: id.clone(), seq: registry.next_seq() });
                let registry = Arc::clone(&registry);
                let dispatcher = Arc::clone(&dispatcher);
                let active_tasks = Arc::clone(&active_tasks);
                let cancellation_tokens = Arc::clone(&cancellation_tokens);
                let cancelled = Arc::clone(&cancelled);
                let tx = tx.clone();
                let key = crate::session::command_key(&id);
                let cancel_token = Arc::new(std::sync::atomic::AtomicBool::new(false));
                if let Ok(mut tokens) = cancellation_tokens.lock() { tokens.insert(key.clone(), Arc::clone(&cancel_token)); }
                let observer = registry.is_observer(&sid);
                let dispatched = tokio::task::spawn_blocking(move || {
                    let parsed = crate::jq::eval(&expr, &bind)?;
                    if observer { check_observer("multi-op", &parsed)?; }
                    let mut payload = parsed;
                    if let serde_json::Value::Object(ref mut map) = payload {
                        map.insert("op".to_string(), serde_json::json!("multi-op"));
                    }
                    dispatcher.dispatch_cancelable("multi-op", workspace.as_deref(), &payload, cancel_token)
                });
                if let Ok(mut tasks) = active_tasks.lock() {
                    tasks.insert(key.clone(), dispatched.abort_handle());
                }
                let broken = Arc::clone(&broken);

                responses.spawn(async move {
                    let dispatched = dispatched.await;
                    let _ = registry.finish(&sid, &id);
                    if let Ok(mut tasks) = active_tasks.lock() { tasks.remove(&key); }
                    if let Ok(mut tokens) = cancellation_tokens.lock() { tokens.remove(&key); }
                    let was_cancelled = cancelled.lock().map(|mut set| set.remove(&key)).unwrap_or(false);
                    if was_cancelled { return; }
                    let frame = match dispatched {
                        Ok(Ok((output, json))) => {
                            let steps = json["checkpoint"]["completed"].as_u64().unwrap_or(0) as usize;
                            let read_only = json["transactional"] == serde_json::json!(false);
                            let output = if registry.note_request(&sid, RequestKind::of(steps, read_only)) { format!("{output}\n{}\n", crate::hint::TEXT) } else { output };
                            Outbound::Result { id: id.clone(), seq: registry.next_seq(), output, json }
                        }
                        Ok(Err(message)) => error_frame(Some(id.clone()), registry.next_seq(), message),
                        Err(join) => error_frame(Some(id.clone()), registry.next_seq(), format!("multi-op panicked: {join}")),
                    };
                    if tx.try_send(frame).is_err() {
                        broken.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
            Inbound::Cancel { id } => {
                let key = crate::session::command_key(&id);
                if let Ok(tokens) = cancellation_tokens.lock() {
                    if let Some(token) = tokens.get(&key) { token.store(true, std::sync::atomic::Ordering::Release); }
                }
                if let Ok(mut tasks) = active_tasks.lock() {
                    if let Some(handle) = tasks.remove(&key) { handle.abort(); }
                }
                if let Ok(mut set) = cancelled.lock() {
                    set.insert(key);
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
                    if let Ok(mut tasks) = active_tasks.lock() {
                        for (_, handle) in tasks.drain() { handle.abort(); }
                    }
                    if let Ok(mut tokens) = cancellation_tokens.lock() {
                        for (_, token) in tokens.drain() { token.store(true, std::sync::atomic::Ordering::Release); }
                    }
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
            Inbound::Enforce { enabled, profile } => {
                if session_id.is_none() {
                    let _ = tx.try_send(error_frame(None, registry.next_seq(), "open a session before enforce".to_string()));
                    continue;
                }
                let (request, label) = match parse_enforce_request(enabled, profile.as_deref()) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        let _ = tx.try_send(error_frame(None, registry.next_seq(), message));
                        continue;
                    }
                };
                match dispatcher.set_enforce(request) {
                    Ok(()) => {
                        let _ = tx.try_send(Outbound::Snapshot {
                            seq: registry.next_seq(),
                            state: serde_json::json!({ "enforce": label }),
                        });
                    }
                    Err(message) => {
                        let _ = tx.try_send(error_frame(None, registry.next_seq(), message));
                    }
                }
            }
            Inbound::Close => {
                graceful_close = true;
                break;
            }
        }
    }

    // Explicit close drains accepted commands; unexpected EOF still cancels.
    if graceful_close {
        while responses.join_next().await.is_some() {}
        let _ = tx.send(Outbound::Snapshot { seq: registry.next_seq(), state: serde_json::json!({"closed": true}) }).await;
    }
    if let Ok(mut tasks) = active_tasks.lock() {
        for (_, handle) in tasks.drain() { handle.abort(); }
    }
    if let Ok(mut tokens) = cancellation_tokens.lock() {
        for (_, token) in tokens.drain() { token.store(true, std::sync::atomic::Ordering::Release); }
    }
    if let Some(sid) = session_id {
        registry.close(&sid);
        crate::trace::record(Some(&sid), serde_json::json!({"phase": "close", "session": &sid}));
        crate::trace::archive(&sid);
    }
    drop(tx);
    let _ = writer.await;
}

// ── unit: the session frame protocol ───────────────────────────────────────
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
            if op == "slow" {
                std::thread::sleep(std::time::Duration::from_millis(50));
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

        // The grammar comes first: a client that has not read it cannot know tags or anchors.
        write_half.write_all(b"{\"do\":\"session.command\",\"id\":6,\"op\":\"context.prompt\"}\n").await.unwrap();
        assert_eq!(next_frame(&mut lines).await["ch"], "ack");
        assert_eq!(next_frame(&mut lines).await["ch"], "result");

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":7,\"op\":\"context.explore\",\"payload\":{\"arg\":\"probe\"}}\n").await.unwrap();
        let ack = next_frame(&mut lines).await;
        assert_eq!(ack["ch"], "ack");
        assert_eq!(ack["id"], 7);
        let result = next_frame(&mut lines).await;
        assert_eq!(result["ch"], "result");
        assert_eq!(result["id"], 7);
        assert!(result["output"].as_str().unwrap().starts_with("ran context.explore"));

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":8,\"op\":\"boom\"}\n").await.unwrap();
        let _ack = next_frame(&mut lines).await;
        let err = next_frame(&mut lines).await;
        assert_eq!(err["ch"], "error");
        assert_eq!(err["id"], 8);
        let diagnostic: Value = serde_json::from_str(err["message"].as_str().unwrap()).unwrap();
        assert_eq!(diagnostic["current"], "boom");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn piped_close_drains_commands_and_multiops_before_closing() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let path = temp_socket();
            let listener = UnixListener::bind(&path).unwrap();
            let registry = Arc::new(SessionRegistry::new());
            tokio::spawn(serve(listener, Arc::clone(&registry), Arc::new(Echo)));
            let stream = UnixStream::connect(&path).await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            write_half.write_all(concat!(
                "{\"do\":\"session.open\",\"label\":\"pipe\"}\n",
                "{\"do\":\"session.command\",\"id\":0,\"op\":\"context.prompt\"}\n",
                "{\"do\":\"session.command\",\"id\":1,\"op\":\"slow\"}\n",
                "{\"do\":\"session.multiop\",\"id\":2,\"expr\":\"{steps: []}\"}\n",
                "{\"do\":\"session.close\"}\n",
            ).as_bytes()).await.unwrap();
            write_half.shutdown().await.unwrap();
            assert_eq!(next_frame(&mut lines).await["ch"], "ready");
            let mut acks = std::collections::HashSet::new();
            let mut results = std::collections::HashSet::new();
            for _ in 0..6 {
                let frame = next_frame(&mut lines).await;
                let id = frame["id"].as_u64().unwrap();
                match frame["ch"].as_str().unwrap() {
                    "ack" => { assert!(acks.insert(id)); }
                    "result" => { assert!(acks.contains(&id)); assert!(results.insert(id)); }
                    _ => panic!("unexpected frame: {frame}"),
                }
            }
            assert_eq!(results, [0, 1, 2].into_iter().collect());
            assert_eq!(next_frame(&mut lines).await["state"]["closed"], true);
            assert!(lines.next_line().await.unwrap().is_none());
            assert_eq!(registry.count(), 0);
            std::fs::remove_file(path).unwrap();
        }).await.expect("piped session hung");
    }

    #[tokio::test]
    async fn observer_checks_evaluated_multiops_before_dispatch() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let path = temp_socket();
            let listener = UnixListener::bind(&path).unwrap();
            let registry = Arc::new(SessionRegistry::new());
            tokio::spawn(serve(listener, registry, Arc::new(Echo)));
            let stream = UnixStream::connect(&path).await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            write_half.write_all(b"{\"do\":\"session.open\",\"label\":\"observer\",\"observer\":true}\n").await.unwrap();
            assert_eq!(next_frame(&mut lines).await["ch"], "ready");
            write_half.write_all(b"{\"do\":\"session.command\",\"id\":99,\"op\":\"context.prompt\"}\n").await.unwrap();
            assert_eq!(next_frame(&mut lines).await["ch"], "ack");
            assert_eq!(next_frame(&mut lines).await["ch"], "result");
            let batches = [
                (json!({"steps":[{"op":"context.read","arg":"a.rs"}]}), "result"),
                (json!({"steps":[{"op":"context.read"},{"op":"mutate.create","path":"escape"}],"validate":"true"}), "error"),
                (json!({"steps":[{"op":"mutate.edit"}]}), "error"),
                (json!({"steps":[{"op":"macro.run"}]}), "error"),
                (json!({"steps":[{"op":"multi-op","steps":[{"op":"mutate.create"}]}]}), "error"),
                (json!({"steps":[{"op":"build"}]}), "error"),
                (json!({"steps":[{"op":"state.up","reindex":true}]}), "error"),
                (json!({"steps":[{"op":"context.read","validate":"touch escape"}]}), "error"),
                (json!({"steps":[{"op":"context.read"}],"validate":"touch escape"}), "error"),
                (json!({"steps":[{"op":"context.read.evil"}]}), "error"),
                (json!({"steps":[{}]}), "error"),
                (json!({"steps":[{"op":"context.prompt"}]}), "result"),
            ];
            for (id, (batch, expected)) in batches.into_iter().enumerate() {
                let frame = json!({"do":"session.multiop","id":id,"expr":"$batch | fromjson","bind":{"batch":batch.to_string()}});
                write_half.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
                assert_eq!(next_frame(&mut lines).await["ch"], "ack");
                let result = next_frame(&mut lines).await;
                assert_eq!(result["ch"], expected, "{result}");
                assert_eq!(result["id"], id);
            }
            // The ordinary-command sibling must not bypass the same observer policy.
            for op in ["build", "mutate.create", "macro.run", "state.up"] {
                let frame = json!({"do":"session.command","id":100,"op":op});
                write_half.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
                assert_eq!(next_frame(&mut lines).await["ch"], "error");
            }
            write_half.write_all(b"{\"do\":\"session.close\"}\n").await.unwrap();
            assert_eq!(next_frame(&mut lines).await["state"]["closed"], true);
            std::fs::remove_file(path).unwrap();
        }).await.expect("observer session hung");
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
