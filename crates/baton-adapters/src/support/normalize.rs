// Rules every reader applies, so that a session reads back the same from any agent it was written to

use baton_core::{Assistant, Body, Image, User};
use serde_json::{Value, json};

pub(crate) struct Content {
    pub(crate) text: String,
    pub(crate) images: Vec<Image>,
}

// Blank messages carry nothing to replay
pub(crate) fn user(content: Content) -> Option<Body> {
    if content.text.trim().is_empty() && content.images.is_empty() {
        return None;
    }
    Some(Body::User(User {
        text: content.text,
        images: content.images,
    }))
}

pub(crate) fn assistant(text: &str) -> Option<Body> {
    if text.trim().is_empty() {
        return None;
    }
    Some(Body::Assistant(Assistant {
        text: text.to_owned(),
    }))
}

// Every provider accepts these characters, so a call keeps its id in every native format
pub(crate) fn call_id(value: &Value) -> String {
    let id: String = value
        .as_str()
        .unwrap_or_default()
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '_' || char == '-' {
                char
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() { "call".into() } else { id }
}

// Tool arguments as an object, the only shape every format can store
pub(crate) fn tool_input(value: Value) -> Value {
    match value {
        Value::Object(_) => value,
        Value::Null => json!({}),
        other => json!({ "input": other }),
    }
}

// Arguments that OpenAI-shaped formats keep as JSON encoded in a string
pub(crate) fn arguments(value: &Value) -> Value {
    match value.as_str() {
        Some(text) => serde_json::from_str(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "input": text })),
        None => tool_input(value.clone()),
    }
}
