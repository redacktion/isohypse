use serde_json::{json, Value};

pub struct FanItem {
    pub tag: String,
    pub render: String,
    pub json: Value,
}

pub trait OpResult {
    fn render(&self) -> String;
    fn to_json(&self) -> Value;
    fn fan_items(&self) -> Option<Vec<FanItem>> {
        None
    }
}

impl OpResult for String {
    fn render(&self) -> String {
        self.clone()
    }
    fn to_json(&self) -> Value {
        json!({ "text": self })
    }
}

/// Canonical machine-readable failure payload used across CLI, daemon, and session surfaces.
pub fn failure_json(layer: &str, step: Option<usize>, operation: Option<&str>, path: Option<&str>, tag: Option<&str>, current: Option<&str>, expected: Option<&str>, replay: Option<&str>, rollback: Option<&str>) -> String {
    json!({
        "kind": "isohypse_failure",
        "layer": layer,
        "step": step,
        "operation": operation,
        "path": path,
        "tag": tag,
        "current": current,
        "expected": expected,
        "replay": replay,
        "rollback": rollback,
    }).to_string()
}
