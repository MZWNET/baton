use baton_core::{Body, Event};

const CLIP: usize = 400;

pub(crate) fn event(event: &Event) -> String {
    let text = match &event.body {
        Body::User(user) if user.images.is_empty() => user.text.clone(),
        Body::User(user) => format!("{} [{} images]", user.text, user.images.len()),
        Body::Assistant(assistant) => assistant.text.clone(),
        Body::ToolCall(call) => {
            let input = clip(&call.input.to_string());
            format!("{} {} {input}", call.name, call.call_id)
        }
        Body::ToolResult(result) => {
            let status = if result.is_error { " error" } else { "" };
            format!("{}{status}: {}", result.call_id, clip(&result.text))
        }
        Body::Compaction(compaction) => compaction.summary.clone(),
        Body::Thinking(thinking) => clip(&thinking.text),
        Body::Meta(meta) => meta.to_string(),
    };
    format!(
        "[{}] {}: {text}",
        minute(&event.timestamp),
        event.body.kind()
    )
}

// A timestamp down to the minute, like 2026-01-01 08:30
pub(crate) fn minute(timestamp: &str) -> String {
    let minute: String = timestamp.chars().take(16).collect();
    minute.replace('T', " ")
}

// Where `term` first occurs in `body`, with some context on both sides and whitespace collapsed
pub(crate) fn snippet(body: &str, term: &str) -> String {
    let at = body
        .to_ascii_lowercase()
        .find(&term.to_ascii_lowercase())
        .unwrap_or(0);
    let start = body[..at]
        .char_indices()
        .rev()
        .nth(40)
        .map_or(0, |(index, _)| index);
    let window: String = body[start..].chars().take(120).collect();
    let text = window.split_whitespace().collect::<Vec<&str>>().join(" ");
    if start > 0 {
        format!("…{text}")
    } else {
        text
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(CLIP) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}
