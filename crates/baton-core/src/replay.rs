//! Which events of a unified session reach an agent when the session is exported, and how to recognize them when the session comes back.

use serde_json::Value;

use crate::{Body, Event, hash};

/// The events an export writes: everything after the last reset, without thinking and meta events.
///
/// Thinking is private or signed by its provider, so it stays in the unified session but is never handed to another agent; meta events describe the session rather than the conversation.
pub fn events<'a>(path: &[&'a Event]) -> Vec<&'a Event> {
    let start = path
        .iter()
        .rposition(|event| event.body.is_reset())
        .map_or(0, |index| index + 1);
    path[start..]
        .iter()
        .copied()
        .filter(|event| event.body.replayable())
        .collect()
}

/// A running hash over event bodies, limited to what every native format can store.
///
/// Two event sequences with equal digests read the same in every agent, which is how an exported prefix is recognized when a native session flows back.
pub fn digest<'a>(bodies: impl IntoIterator<Item = &'a Body>) -> String {
    bodies.into_iter().fold(String::new(), |previous, body| {
        let portable = hash::canonical_json(&portable(body));
        hash::sha256_hex(format!("{previous}{portable}").as_bytes())
    })
}

// Codex stores no error flag on tool results, so it cannot take part in the comparison
fn portable(body: &Body) -> Value {
    let mut value = serde_json::to_value(body).expect("event bodies serialize to JSON");
    if let Body::ToolResult(_) = body {
        value["payload"]
            .as_object_mut()
            .expect("tool results serialize to objects")
            .remove("is_error");
    }
    value
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Agent, Assistant, Draft, Thinking, ToolResult, User};

    fn draft(body: Body) -> Draft {
        Draft {
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            native_entry_id: None,
            body,
        }
    }

    fn user(text: &str) -> Body {
        Body::User(User {
            text: text.into(),
            images: vec![],
        })
    }

    #[test]
    fn replay_drops_private_state_and_everything_before_the_last_reset() {
        let bodies = [
            user("before reset"),
            Body::Meta(json!({"type": "reset"})),
            user("after reset"),
            Body::Thinking(Thinking {
                text: "private".into(),
                signature: Some("signed".into()),
            }),
            Body::Meta(json!({"type": "model", "model": "m"})),
            Body::Assistant(Assistant {
                text: "answer".into(),
            }),
        ];
        let chained = Event::chain(bodies.into_iter().map(draft), None, Agent::Omp, "n");
        let path: Vec<&Event> = chained.iter().collect();
        let replayed: Vec<String> = events(&path)
            .iter()
            .map(|event| event.body.text())
            .collect();
        assert_eq!(replayed, ["after reset", "answer"]);
    }

    #[test]
    fn digest_ignores_tool_error_flags() {
        let result = |is_error| {
            Body::ToolResult(ToolResult {
                call_id: "c".into(),
                text: "out".into(),
                images: vec![],
                is_error,
            })
        };
        assert_eq!(digest([&result(true)]), digest([&result(false)]));
        assert_ne!(digest([&result(true)]), digest([&user("out")]));
    }
}
