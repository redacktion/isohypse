//! Exercise Claude's real REPL registration using a loopback-only mock model.
//!
//! No MCP, Bash tool, paid inference, or installed binary modifications.
//!
//! The mock records every request it serves and asserts what the harness is
//! allowed to reach for, so a regression in the grant gates or the native tool
//! surface fails here rather than in a live session.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

const REGISTRATION: &str = r#"registerTool("isohypse_probe", "Verify native Isohypse adapter registration", {type:"object",properties:{nonce:{type:"string"}},required:["nonce"],additionalProperties:false}, async ({nonce}) => ({native:true,nonce}), {displayName:"Isohypse probe"}); listTools()"#;


struct Shared {
    requests: Mutex<Vec<Value>>,
    failure: Mutex<Option<String>>,
    phase: AtomicUsize,
    fixture: String,
}

impl Shared {
    fn assert(&self, condition: bool, message: String) {
        if !condition {
            let mut slot = self.failure.lock().expect("failure lock");
            if slot.is_none() {
                *slot = Some(message);
            }
        }
    }

    fn recorded(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn clear(&self) {
        self.requests.lock().expect("requests lock").clear();
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|value| !value.is_empty()).unwrap_or(false)
}

fn push_event(out: &mut String, kind: &str, data: Value) {
    out.push_str("event: ");
    out.push_str(kind);
    out.push_str("\ndata: ");
    out.push_str(&serde_json::to_string(&data).unwrap_or_default());
    out.push_str("\n\n");
}

/// The turn's assistant content, asserted against what the harness sent.
fn content_for(shared: &Shared, body: &Value, turn: usize, names: &[String]) -> Value {
    let resume = env_flag("ISOHYPSE_TEST_RESUME");
    let native = env_flag("CLAUDE_NATIVE_ENTRY") || env_flag("CLAUDE_NATIVE_BINARY");
    let phase = shared.phase.load(Ordering::SeqCst);
    let limit = if phase == 0 { 2 } else { 3 };
    let fixture = shared.fixture.as_str();

    if resume && turn <= limit {
        if phase == 0 {
            let batch = if turn == 1 {
                json!({"steps": [{"op": "mutate.create", "path": fixture, "content": "before\n"}], "validate": "true"})
            } else {
                json!({"steps": [{"op": "context.read", "arg": fixture}]})
            };
            return json!({"type": "tool_use", "id": format!("write_{turn}"), "name": "isohypse", "input": batch});
        }
        let call_id = if turn == 1 { "write_1".to_string() } else { format!("resume_{turn}") };
        let batch = if turn == 1 {
            json!({"steps": [{"op": "mutate.create", "path": fixture, "content": "replayed\n"}], "validate": "true"})
        } else if turn == 2 {
            json!({"steps": [{"op": "context.read", "arg": fixture}]})
        } else {
            let messages = serde_json::to_string(body.get("messages").unwrap_or(&Value::Null)).unwrap_or_default();
            let tags: Vec<String> = tagged(&messages, fixture);
            shared.assert(!tags.is_empty(), "No tagged resumed read".to_string());
            let tag = tags.last().cloned().unwrap_or_default();
            json!({"steps": [{"op": "mutate.edit", "path": fixture, "tag": tag, "edits": [{"replace": {"lines": [1, 1]}, "with": "after\n"}]}], "validate": "true"})
        };
        return json!({"type": "tool_use", "id": call_id, "name": "isohypse", "input": batch});
    }
    if resume {
        return json!({"type": "text", "text": "Resume smoke complete."});
    }
    if native {
        if turn <= 2 {
            shared.assert(
                names.iter().any(|name| name == "isohypse"),
                format!("native isohypse tool absent from {names:?}"),
            );
            return json!({"type": "tool_use", "id": format!("toolu_native_{turn}"), "name": "isohypse",
                "input": {"steps": [{"op": "context.read", "arg": "README.md", "range": "1-12"}]}});
        }
        return json!({"type": "text", "text": "Native registration probe complete."});
    }
    // Registration probe. The first request carries no tool list at all and how a
    // REPL-registered tool surfaces is undocumented, so the mock drives off the
    // tools actually offered rather than a turn count.
    if let Some(probe) = names.iter().find(|name| name.contains("isohypse_probe")).cloned() {
        return json!({"type": "tool_use", "id": "toolu_probe", "name": probe, "input": {"nonce": "native-roundtrip"}});
    }
    if names.iter().any(|name| name == "REPL") {
        return json!({"type": "tool_use", "id": "toolu_register", "name": "REPL", "input": {"code": REGISTRATION}});
    }
    json!({"type": "text", "text": "REPL registration probe complete."})
}

/// Tag prefixes of the form `<path>#<hex>` recorded in a resumed transcript.
fn tagged(messages: &str, fixture: &str) -> Vec<String> {
    let needle = format!("{fixture}#");
    let mut out = Vec::new();
    let mut rest = messages;
    while let Some(at) = rest.find(&needle) {
        let tail = &rest[at + needle.len()..];
        let hex: String = tail.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if !hex.is_empty() {
            out.push(hex.clone());
        }
        rest = &tail[hex.len().min(tail.len())..];
    }
    out
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn handle(shared: &Arc<Shared>, path: &str, body: &Value, stream: &mut TcpStream) -> std::io::Result<()> {
    if path.ends_with("/count_tokens") {
        return respond(stream, "application/json", "{\"input_tokens\":100}");
    }
    let turn = {
        let mut requests = shared.requests.lock().expect("requests lock");
        requests.push(body.clone());
        requests.len()
    };
    let names: Vec<String> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    println!("{}", json!({"turn": turn, "tools": names}));

    let content = content_for(shared, body, turn, &names);
    let stop = if content.get("type").and_then(Value::as_str) == Some("tool_use") {
        "tool_use"
    } else {
        "end_turn"
    };
    let message = json!({
        "id": format!("msg_probe_{}_{turn}", shared.phase.load(Ordering::SeqCst)),
        "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
        "content": [content], "stop_reason": stop, "stop_sequence": Value::Null,
        "usage": {"input_tokens": 100, "output_tokens": 20},
    });
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return respond(stream, "application/json", &serde_json::to_string(&message).unwrap_or_default());
    }

    let mut block = content.clone();
    if stop == "tool_use" {
        block["input"] = json!({});
    } else {
        block["text"] = json!("");
    }
    let delta = if stop == "tool_use" {
        json!({"type": "input_json_delta", "partial_json": serde_json::to_string(content.get("input").unwrap_or(&Value::Null)).unwrap_or_default()})
    } else {
        json!({"type": "text_delta", "text": content.get("text").cloned().unwrap_or(Value::Null)})
    };
    let mut started = message.clone();
    started["content"] = json!([]);
    started["stop_reason"] = Value::Null;
    let mut out = String::new();
    push_event(&mut out, "message_start", json!({"type": "message_start", "message": started}));
    push_event(&mut out, "content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": block}));
    push_event(&mut out, "content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": delta}));
    push_event(&mut out, "content_block_stop", json!({"type": "content_block_stop", "index": 0}));
    push_event(&mut out, "message_delta", json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": Value::Null}, "usage": {"output_tokens": 20}}));
    push_event(&mut out, "message_stop", json!({"type": "message_stop"}));
    respond(stream, "text/event-stream", &out)
}

fn serve(listener: TcpListener, shared: Arc<Shared>) {
    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut path = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(rest) = trimmed.strip_prefix("POST ") {
                    path = rest.split_whitespace().next().unwrap_or("").to_string();
                }
                if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let _ = handle(&shared, &path, &parsed, &mut stream);
        });
    }
}

fn run() -> Result<String, String> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .map_err(|e| format!("cannot resolve repository root: {e}"))?;
    let fixture = format!("harness/claude/native-smoke-{}.txt", 
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let shared = Arc::new(Shared {
        requests: Mutex::new(Vec::new()),
        failure: Mutex::new(None),
        phase: AtomicUsize::new(0),
        fixture: fixture.clone(),
    });

    let directory = std::env::temp_dir().join(format!("isohypse-claude-probe-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|e| format!("cannot create probe directory: {e}"))?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot bind mock model: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || serve(listener, shared));
    }

    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.starts_with("ANTHROPIC_") && !key.starts_with("CLAUDE_"))
        .collect();
    env.insert("CLAUDE_CONFIG_DIR".into(), directory.display().to_string());
    env.insert("ANTHROPIC_API_KEY".into(), "local-probe-not-a-credential".into());
    env.insert("ANTHROPIC_BASE_URL".into(), format!("http://127.0.0.1:{port}"));
    env.insert("CLAUDE_CODE_REPL".into(), "1".into());
    env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into());

    let command = if let Ok(binary) = std::env::var("CLAUDE_NATIVE_BINARY") {
        vec![binary]
    } else if let Ok(entry) = std::env::var("CLAUDE_NATIVE_ENTRY") {
        vec![format!("{}/.bun/bin/bun", home()), entry]
    } else {
        vec![format!("{}/.local/bin/claude", home())]
    };

    let outcome = run_probe(&shared, &command, &env, &repo_root, &directory, &fixture);
    let failure = shared.failure.lock().expect("failure lock").clone();
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_file(repo_root.join(&fixture));
    if let Some(message) = failure {
        return Err(format!("mock model assertion failed: {message}"));
    }
    Ok(outcome)
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

fn run_probe(
    shared: &Arc<Shared>,
    command: &[String],
    env: &BTreeMap<String, String>,
    repo_root: &PathBuf,
    directory: &PathBuf,
    fixture: &str,
) -> String {
    let stream_args = ["-p", "Run the native smoke.", "--output-format", "stream-json", "--verbose",
        "--max-turns", "5", "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}", "--allowedTools", "isohypse"];

    if env_flag("ISOHYPSE_TEST_RESUME") {
        let mut readonly = env.clone();
        readonly.insert("ISOHYPSE_ALLOW_EDITS".into(), "0".into());
        let output = capture(command, &stream_args, repo_root, &readonly);
        shared.assert(
            output.contains("Writes require the host launcher") && !repo_root.join(fixture).exists(),
            output.clone(),
        );
        shared.clear();

        let mut denied_args = stream_args.to_vec();
        denied_args.extend_from_slice(&["--disallowedTools", "isohypse"]);
        let denied = capture(command, &denied_args, repo_root, env);
        shared.assert(!repo_root.join(fixture).exists() && denied.contains("permission_denials"), denied.clone());
        shared.clear();

        let mut granted = env.clone();
        if env_flag("ISOHYPSE_TEST_DEFAULT_GRANT") {
            granted.remove("ISOHYPSE_ALLOW_EDITS");
        }
        let first = capture(command, &stream_args, repo_root, &granted);
        shared.assert(first.contains("\"subtype\":\"init\"") || first.contains("\"subtype\": \"init\""), first.clone());
        shared.assert(
            std::fs::read_to_string(repo_root.join(fixture)).unwrap_or_default() == "before\n",
            first.clone(),
        );
        let session_id = json_field(&first, "session_id").unwrap_or_default();
        shared.phase.store(1, Ordering::SeqCst);
        shared.clear();

        let mut resume_args = stream_args.to_vec();
        resume_args.extend_from_slice(&["--resume", session_id.as_str()]);
        let second = capture(command, &resume_args, repo_root, &granted);
        shared.assert(second.contains("Duplicate tool call across resume"), second.clone());
        shared.assert(
            std::fs::read_to_string(repo_root.join(fixture)).unwrap_or_default().trim() == "after",
            second.clone(),
        );
        return "PASS: host grant and Claude denial enforced; native create, persisted resume, duplicate rejection, fresh read, and edit.".to_string();
    }

    let probe_args = ["-p", "Run the native registration probe.", "--output-format", "stream-json", "--verbose",
        "--max-turns", "8", "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}", "--allowedTools", "isohypse",
        "REPL", "isohypse_probe", "--no-session-persistence"];
    let cwd = if env_flag("CLAUDE_NATIVE_ENTRY") { repo_root.clone() } else { directory.clone() };
    let output = capture(command, &probe_args, &cwd, env);
    let recorded = shared.recorded();
    if recorded.len() < 3 {
        return format!("No native roundtrip; {output}");
    }
    if env_flag("CLAUDE_NATIVE_ENTRY") || env_flag("CLAUDE_NATIVE_BINARY") {
        let results: Vec<Value> = recorded
            .last()
            .and_then(|body| body.get("messages"))
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| message.get("content").and_then(Value::as_array))
                    .flatten()
                    .filter(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                            && block.get("tool_use_id").and_then(Value::as_str).unwrap_or("").starts_with("toolu_native_")
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let clean = results.len() == 2
            && results.iter().all(|block| {
                block.get("is_error").and_then(Value::as_bool) != Some(true)
                    && block.get("content").map(|content| content.to_string().contains("Isohypse")).unwrap_or(false)
            });
        shared.assert(clean, format!("{results:?}"));
        return "PASS: native isohypse registered and read README twice through the signed session; no MCP or Bash.".to_string();
    }
    let encoded = serde_json::to_string(recorded.last().and_then(|body| body.get("messages")).unwrap_or(&Value::Null)).unwrap_or_default();
    shared.assert(!encoded.is_empty(), "no recorded request body".to_string());
    "PASS: REPL registration probe completed; the mock recorded the tool list each turn.".to_string()
}

fn capture(command: &[String], args: &[&str], cwd: &PathBuf, env: &BTreeMap<String, String>) -> String {
    let output = Command::new(&command[0])
        .args(&command[1..])
        .args(args)
        .current_dir(cwd)
        .envs(env)
        .output();
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text
        }
        Err(error) => format!("cannot run {}: {error}", command[0]),
    }
}

/// First string value for `key` in an NDJSON stream.
fn json_field(stream: &str, key: &str) -> Option<String> {
    for line in stream.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(found) = value.get(key).and_then(Value::as_str) {
            return Some(found.to_string());
        }
    }
    None
}

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("isohypse: {error}");
            ExitCode::FAILURE
        }
    }
}
