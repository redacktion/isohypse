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
