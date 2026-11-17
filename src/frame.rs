use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resume {
    pub session: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Inbound {
    #[serde(rename = "session.open")]
    Open {
        label: String,
        #[serde(default)]
        resume: Option<Resume>,
        #[serde(default)]
        observer: bool,
    },
    #[serde(rename = "session.command")]
    Command {
        id: Value,
        op: String,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        payload: Value,
    },
    #[serde(rename = "session.cancel")]
    Cancel {
        id: Value,
    },
    #[serde(rename = "session.subscribe")]
    Subscribe {
        #[serde(default)]
        kinds: Vec<String>,
        #[serde(default)]
        workspaces: Vec<String>,
    },
    #[serde(rename = "session.unsubscribe")]
    Unsubscribe,
    #[serde(rename = "session.request")]
    Request {
        body: String,
        #[serde(default)]
        scope: Option<String>,
    },
    #[serde(rename = "session.requests")]
    Requests,
    #[serde(rename = "session.reload")]
    Reload,
    #[serde(rename = "session.close")]
    Close,
    #[serde(rename = "session.multiop")]
    MultiOp {
        id: Value,
        expr: String,
        #[serde(default)]
        bind: std::collections::HashMap<String, String>,
        #[serde(default)]
        workspace: Option<String>,
    },
    #[serde(rename = "session.enforce")]
    Enforce {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        profile: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ch", rename_all = "snake_case")]
pub enum Outbound {
    Ready {
        session: String,
        token: String,
        seq: u64,
    },
    Snapshot {
        seq: u64,
        state: Value,
    },
    Ack {
        id: Value,
        seq: u64,
    },
    Stream {
        id: Value,
        seq: u64,
        chunk: Value,
    },
    Result {
        id: Value,
        seq: u64,
        output: String,
        json: Value,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        seq: u64,
        message: String,
        #[serde(default, skip_serializing_if = "is_false")]
        retry: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        backoff_ms: Option<u64>,
    },
    Cancelled {
        id: Value,
        seq: u64,
    },
    Event {
        seq: u64,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        payload: Value,
    },
    Gap {
        seq: u64,
        dropped: u64,
    },
    Finding {
        seq: u64,
        session: String,
        severity: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recommendation: Option<String>,
    },
}

impl Inbound {
    pub fn parse(line: &str) -> Result<Inbound, String> {
        serde_json::from_str(line).map_err(|e| format!("malformed frame: {e}"))
    }
}

impl Outbound {
    pub fn line(&self) -> String {
        let mut text = serde_json::to_string(self).unwrap_or_else(|e| {
            let escaped = Value::String(e.to_string());
            format!("{{\"ch\":\"error\",\"seq\":0,\"message\":{escaped}}}")
        });
        text.push('\n');
        text
    }
}

pub fn command_payload(payload: &Value) -> Map<String, Value> {
    match payload {
        Value::Object(map) => map.clone(),
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    }
}
