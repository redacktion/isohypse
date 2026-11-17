//! `isohypse call`: run one batch in a fresh session. `Batch::parse` is the only
//! place untrusted input becomes typed. `frames` and `outcome` are pure.
//! `run` is the one function that touches a socket.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

const MAX_STEPS: usize = 32;
const PROMPT_ID: u64 = 0;
const BATCH_ID: u64 = 1;
const READ_TIMEOUT: Duration = Duration::from_secs(900);
/// The daemon accepts 8 MiB requests; stay under it.
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 - 65536;

const USAGE: &str = "expected {\"steps\":[{\"op\":\"context.read\",\"arg\":\"path\"}]}; text fields (with, body, content) may be a string or an array of lines";

#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    TooLarge(usize),
    InvalidJson(String),
    BadShape(&'static str),
    UnknownField(String),
    StepCount(usize),
    MissingOp(usize),
    Connect(String),
    Untrusted,
    Io(String),
    Failure(String),
    Unconfirmed,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::TooLarge(n) => write!(f, "batch is {n} bytes, over the {MAX_INPUT_BYTES} byte limit; split it across several calls"),
            CallError::InvalidJson(e) => write!(f, "batch is not valid JSON ({e}); escape quotes and newlines inside strings; {USAGE}"),
            CallError::BadShape(what) => write!(f, "{what}; {USAGE}"),
            CallError::UnknownField(key) => write!(f, "unknown batch field {key:?}; {USAGE}"),
            CallError::StepCount(n) => write!(f, "use 1 to {MAX_STEPS} steps, got {n}"),
            CallError::MissingOp(n) => write!(f, "step {n} has no \"op\"; {USAGE}"),
            CallError::Connect(e) => write!(f, "cannot connect to session socket: {e}"),
            CallError::Untrusted => write!(f, "session socket failed code-signature verification"),
            CallError::Io(e) => write!(f, "cannot talk to the session: {e}"),
            CallError::Failure(text) => write!(f, "{text}"),
            CallError::Unconfirmed => write!(f, "session closed before the batch finished; outcome unconfirmed, inspect files before retrying"),
        }
    }
}

/// One op with its fields. The op name is always present and non-empty.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    op: String,
    fields: Map<String, Value>,
}

impl Step {
    fn parse(index: usize, value: Value) -> Result<Step, CallError> {
        let Value::Object(mut fields) = value else { return Err(CallError::MissingOp(index + 1)) };
        match fields.remove("op") {
            Some(Value::String(op)) if !op.is_empty() => {
                let mut value = Value::Object(fields);
                join_text_lines(&mut value);
                let Value::Object(fields) = value else { return Err(CallError::MissingOp(index + 1)) };
                Ok(Step { op, fields })
            }
            _ => Err(CallError::MissingOp(index + 1)),
        }
    }

    pub fn op(&self) -> &str {
        &self.op
    }

    fn to_value(&self) -> Value {
        let mut map = self.fields.clone();
        map.insert("op".to_string(), Value::String(self.op.clone()));
        Value::Object(map)
    }
}

/// 1 to MAX_STEPS steps, and an optional validation command.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    steps: Vec<Step>,
    validate: Option<Value>,
}

/// `with`, `body`, and `content` accept an array of lines, so code needs no \n escapes.
fn join_text_lines(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let lines: Option<Vec<&str>> = match child {
                    Value::Array(items) if matches!(key.as_str(), "with" | "body" | "content") => items.iter().map(Value::as_str).collect(),
                    _ => None,
                };
                match lines {
                    Some(lines) => {
                        let mut text = lines.join("\n");
                        if key == "content" { text.push('\n'); }
                        *child = Value::String(text);
                    }
                    None => join_text_lines(child),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(join_text_lines),
        _ => {}
    }
}

impl Batch {
    /// Accepts {steps, validate}, one bare step, or an array of steps.
    pub fn parse(input: &str) -> Result<Batch, CallError> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(CallError::TooLarge(input.len()));
        }
        let value: Value = serde_json::from_str(input).map_err(|e| CallError::InvalidJson(e.to_string()))?;
        let (steps, validate) = match value {
            Value::Array(steps) => (steps, None),
            Value::Object(mut map) if map.contains_key("op") && !map.contains_key("steps") => {
                let validate = map.remove("validate");
                (vec![Value::Object(map)], validate)
            }
            Value::Object(mut map) => {
                let validate = map.remove("validate");
                if let Some(extra) = map.keys().find(|key| key.as_str() != "steps") {
                    return Err(CallError::UnknownField(extra.clone()));
                }
                match map.remove("steps") {
                    Some(Value::Array(steps)) => (steps, validate),
                    Some(_) => return Err(CallError::BadShape("steps must be an array")),
                    None => return Err(CallError::BadShape("batch needs a steps array")),
                }
            }
            _ => return Err(CallError::BadShape("batch must be a JSON object or array")),
        };
        if steps.is_empty() || steps.len() > MAX_STEPS {
            return Err(CallError::StepCount(steps.len()));
        }
        let steps = steps.into_iter().enumerate().map(|(i, step)| Step::parse(i, step)).collect::<Result<Vec<_>, _>>()?;
        Ok(Batch { steps, validate })
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn validate(&self) -> Option<&Value> {
        self.validate.as_ref()
    }
}

/// The NDJSON frames for one call, in send order.
pub fn frames(batch: &Batch) -> Vec<Value> {
    let mut out = vec![
        json!({"do": "session.open", "label": "call"}),
        json!({"do": "session.command", "id": PROMPT_ID, "op": "context.prompt"}),
    ];
    match (batch.steps.as_slice(), &batch.validate) {
        ([only], None) => out.push(json!({"do": "session.command", "id": BATCH_ID, "op": only.op, "payload": Value::Object(only.fields.clone())})),
        (steps, validate) => {
            let mut body = json!({"steps": steps.iter().map(Step::to_value).collect::<Vec<_>>()});
            if let Some(validate) = validate {
                body["validate"] = validate.clone();
            }
            out.push(json!({"do": "session.multiop", "id": BATCH_ID, "expr": "$batch | fromjson", "bind": {"batch": body.to_string()}}));
        }
    }
    out.push(json!({"do": "session.close"}));
    out
}

fn failure_text(message: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(message) else { return message.to_string() };
    if !map.contains_key("kind") {
        return message.to_string();
    }
    let field = |key: &str| map.get(key).and_then(Value::as_str);
    let head: Vec<String> = [("op", "operation"), ("path", "path"), ("tag", "tag")]
        .iter()
        .filter_map(|(label, key)| field(key).map(|v| format!("{label} {v}")))
        .collect();
    let mut text = match (head.is_empty(), field("current")) {
        (true, Some(current)) => current.to_string(),
        (false, Some(current)) => format!("{}: {current}", head.join(", ")),
        _ => message.to_string(),
    };
    if let Some(expected) = field("expected") {
        text.push_str(&format!("\nexpected: {expected}"));
    }
    if let Some(rollback) = field("rollback") {
        text.push_str(&format!("\n{rollback}"));
    }
    text
}

/// Interpret the reply stream: the batch output, or why it failed.
pub fn outcome(lines: impl Iterator<Item = String>) -> Result<String, CallError> {
    for line in lines {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
        let id = frame.get("id").and_then(Value::as_u64);
        match frame.get("ch").and_then(Value::as_str) {
            Some("result") if id == Some(BATCH_ID) => {
                return Ok(frame.get("output").and_then(Value::as_str).unwrap_or_default().to_string());
            }
            Some("error") if id == Some(BATCH_ID) || id == Some(PROMPT_ID) || id.is_none() => {
                let text = failure_text(frame.get("message").and_then(Value::as_str).unwrap_or("unknown error"));
                let denied_prompt = id == Some(PROMPT_ID) && text.contains("enforce");
                return Err(CallError::Failure(if denied_prompt {
                    format!("{text}\nthe workspace policy must allow context.prompt for `isohypse call`")
                } else {
                    text
                }));
            }
            _ => {}
        }
    }
    Err(CallError::Unconfirmed)
}

/// Run one batch against a live session socket.
pub fn run(socket: &Path, input: &str) -> Result<String, CallError> {
    let batch = Batch::parse(input)?;
    let stream = UnixStream::connect(socket).map_err(|e| CallError::Connect(e.to_string()))?;
    {
        use std::os::unix::io::AsRawFd;
        if !crate::trust::peer_signature_ok(stream.as_raw_fd()) {
            return Err(CallError::Untrusted);
        }
    }
    stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| CallError::Io(e.to_string()))?;
    let mut writer = stream.try_clone().map_err(|e| CallError::Io(e.to_string()))?;
    for frame in frames(&batch) {
        writeln!(writer, "{frame}").map_err(|e| CallError::Io(e.to_string()))?;
    }
    outcome(BufReader::new(stream).lines().map_while(Result::ok))
}
