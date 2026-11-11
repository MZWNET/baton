// Rules every reader applies, so that a session reads back the same from any agent it was written to

use anyhow::{Result, bail};
use baton_core::{Assistant, Body, Image, User, portable_call_id};
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

// These formats require ids to pair calls with their results
pub(crate) fn call_id(value: &Value) -> Result<String> {
    match value.as_str() {
        Some(id) if !id.is_empty() => Ok(portable_call_id(id)),
        _ => bail!("a tool call or result has no id"),
    }
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
