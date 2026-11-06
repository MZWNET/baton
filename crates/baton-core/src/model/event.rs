use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Agent, Body, hash};

const FORMAT_VERSION: u32 = 1;

/// One event in a compressed session segment.
///
/// Events form a tree through `parent`, and the active branch ends at the newest leaf. An id hashes the body together with the parent id, so equal content with equal history always gets the same id, whichever agent it was imported from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The version of the event format.
    #[serde(rename = "v")]
    pub version: u32,
    /// SHA-256 of the canonical JSON of `kind`, `payload` and `parent`.
    pub id: String,
    /// The id of the previous event, or `None` for the first one.
    pub parent: Option<String>,
    /// When the event happened, as an RFC 3339 timestamp in UTC.
    #[serde(rename = "ts")]
    pub timestamp: String,
    /// Where the event was imported from.
    pub origin: Origin,
    /// What the event says.
    #[serde(flatten)]
    pub body: Body,
}

/// The native session an event was imported from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// The agent of the native session.
    pub agent: Agent,
    /// The agent's id for the native session.
    pub native_id: String,
    /// The agent's id for the entry the event came from, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_entry_id: Option<String>,
}

/// An event read from a native session, before it is placed in a chain and given an id.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// When the event happened, as an RFC 3339 timestamp in UTC.
    pub timestamp: String,
    /// The agent's id for the entry the event came from.
    pub native_entry_id: Option<String>,
    /// What the event says.
    pub body: Body,
}

impl Event {
    pub(crate) fn id_for(parent: Option<&str>, body: &Body) -> String {
        let mut value = serde_json::to_value(body).expect("event bodies serialize to JSON");
        value["parent"] = parent.map_or(Value::Null, |parent| Value::String(parent.to_owned()));
        hash::sha256_hex(hash::canonical_json(&value).as_bytes())
    }

    /// Turns drafts into a chain of events following `parent`, each event the parent of the next.
    ///
    /// ```
    /// use baton_core::{Agent, Assistant, Body, Draft, Event};
    ///
    /// let draft = |text: &str| Draft {
    ///     timestamp: "2026-01-01T00:00:00.000Z".into(),
    ///     native_entry_id: None,
    ///     body: Body::Assistant(Assistant { text: text.into() }),
    /// };
    /// let events = Event::chain([draft("a"), draft("b")], None, Agent::Pi, "session");
    /// assert_eq!(events[1].parent.as_deref(), Some(events[0].id.as_str()));
    /// ```
    pub fn chain(
        drafts: impl IntoIterator<Item = Draft>,
        mut parent: Option<String>,
        agent: Agent,
        native_id: &str,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for draft in drafts {
            let id = Event::id_for(parent.as_deref(), &draft.body);
            events.push(Event {
                version: FORMAT_VERSION,
                parent: parent.replace(id.clone()),
                id,
                timestamp: draft.timestamp,
                origin: Origin {
                    agent,
                    native_id: native_id.to_owned(),
                    native_entry_id: draft.native_entry_id,
                },
                body: draft.body,
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::User;

    fn user(text: &str) -> Draft {
        Draft {
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            native_entry_id: None,
            body: Body::User(User {
                text: text.into(),
                images: vec![],
            }),
        }
    }

    #[test]
    fn ids_depend_on_content_and_ancestry_only() {
        let first = Event::chain([user("a"), user("b")], None, Agent::Pi, "x");
        let mut moved = user("a");
        moved.timestamp = "2030-01-01T00:00:00.000Z".into();
        let second = Event::chain([moved, user("b")], None, Agent::Claude, "y");
        assert_eq!(
            first[1].id, second[1].id,
            "the same history imported from another agent must deduplicate"
        );
        let rooted = Event::chain([user("b")], None, Agent::Pi, "x");
        assert_ne!(
            rooted[0].id, first[1].id,
            "an id must commit to the whole history before the event"
        );
    }

    #[test]
    fn events_serialize_in_plan_field_order() {
        let event = &Event::chain([user("hi")], None, Agent::Omp, "n")[0];
        let line = serde_json::to_string(event).unwrap();
        assert!(line.starts_with(r#"{"v":1,"id":""#), "{line}");
        assert!(
            line.contains(r#","ts":"2026-01-01T00:00:00.000Z","origin":{"agent":"omp","native_id":"n"},"kind":"user","payload":{"text":"hi"}}"#),
            "{line}"
        );
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), *event);
    }
}
