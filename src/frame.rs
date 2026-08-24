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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_frame_round_trips_with_resume() {
        let line = r#"{"do":"session.open","label":"codex-ui-1","resume":{"session":"codex-ui-1.7f3a","token":"abc"}}"#;
        let parsed = Inbound::parse(line).unwrap();
        match parsed {
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
    fn command_frame_carries_id_and_payload() {
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
    fn error_frame_omits_false_retry_and_none_fields() {
        let frame = Outbound::Error {
            id: None,
            seq: 3,
            message: "boom".to_string(),
            retry: false,
            backoff_ms: None,
        };
        let line = frame.line();
        assert!(line.contains("\"ch\":\"error\""), "{line}");
        assert!(!line.contains("retry"), "{line}");
        assert!(!line.contains("backoff_ms"), "{line}");
        assert!(!line.contains("\"id\""), "{line}");
    }

    #[test]
    fn retry_error_frame_includes_backoff() {
        let frame = Outbound::Error {
            id: Some(json!(2)),
            seq: 9,
            message: "rolling over".to_string(),
            retry: true,
            backoff_ms: Some(250),
        };
        let line = frame.line();
        assert!(line.contains("\"retry\":true"), "{line}");
        assert!(line.contains("\"backoff_ms\":250"), "{line}");
    }
}
