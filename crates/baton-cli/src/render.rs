use std::borrow::Cow;
use std::io::{self, Write};

use baton_core::{Body, Event};

const CLIP: usize = 400;

pub(crate) fn event(output: &mut impl Write, event: &Event) -> io::Result<()> {
    write!(
        output,
        "[{}] {}: ",
        minute(&event.timestamp),
        event.body.kind()
    )?;
    match &event.body {
        Body::User(user) if user.images.is_empty() => write!(output, "{}", user.text),
        Body::User(user) => write!(output, "{} [{} images]", user.text, user.images.len()),
        Body::Assistant(assistant) => write!(output, "{}", assistant.text),
        Body::ToolCall(call) => {
            let input = call.input.to_string();
            let input = clip(&input);
            write!(output, "{} {} {input}", call.name, call.call_id)
        }
        Body::ToolResult(result) => {
            let status = if result.is_error { " error" } else { "" };
            write!(output, "{}{status}: {}", result.call_id, clip(&result.text))
        }
        Body::Compaction(compaction) => write!(output, "{}", compaction.summary),
        Body::Thinking(thinking) => write!(output, "{}", clip(&thinking.text)),
        Body::Meta(meta) => write!(output, "{meta}"),
    }
}

// A timestamp down to the minute, like 2026-01-01 08:30
pub(crate) fn minute(timestamp: &str) -> String {
    let minute: String = timestamp.chars().take(16).collect();
    minute.replace('T', " ")
}

// The first 120 characters, with whitespace collapsed
pub(crate) fn snippet(body: &str) -> String {
    let window: String = body.chars().take(120).collect();

    window.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn clip(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(CLIP) {
        Some((end, _)) => Cow::Owned(format!("{}…", &text[..end])),
        None => Cow::Borrowed(text),
    }
}
