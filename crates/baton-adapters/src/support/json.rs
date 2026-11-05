use baton_core::time;
use serde_json::Value;

// A non-empty string field
pub(crate) fn string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(String::from)
}

// The texts of the blocks of the given types, joined by newlines
pub(crate) fn texts(blocks: &[Value], kinds: &[&str]) -> String {
    let mut texts = Vec::new();
    for block in blocks {
        let wanted = block["type"]
            .as_str()
            .is_some_and(|kind| kinds.contains(&kind));
        if wanted && let Some(text) = block["text"].as_str() {
            texts.push(text);
        }
    }
    texts.join("\n")
}

pub(crate) fn timestamp(value: &Value, fallback: &str) -> String {
    value
        .as_str()
        .and_then(time::normalize)
        .unwrap_or_else(|| fallback.to_owned())
}

pub(crate) fn timestamp_from_millis(value: &Value, fallback: &str) -> String {
    value
        .as_i64()
        .and_then(time::from_unix_millis)
        .unwrap_or_else(|| fallback.to_owned())
}
