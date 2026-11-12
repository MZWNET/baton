//! Which events of a unified session reach an agent when the session is exported, and how to recognize them when the session comes back.

use std::collections::HashMap;

use serde_json::Value;

use crate::{Body, Event, hash};

// What Claude Code records as the model of messages no model produced, which old pi data also stores as a model, and what baton writes for a provider or model the session never named
const SYNTHETIC: &str = "<synthetic>";

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
        hash::canonical_sha256(previous.as_bytes(), &portable(body))
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

/// A model a meta event of type `model` names
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model<'a> {
    /// The provider, when the agent recorded it apart from the model
    pub provider: Option<&'a str>,
    /// The model id; agents that record no provider apart may prefix it with one and a slash
    pub id: &'a str,
}

impl<'a> Model<'a> {
    /// The provider and the bare model id, taking the provider from before the first slash of the id when none was recorded apart
    pub fn split(&self) -> (Option<&'a str>, &'a str) {
        match self.id.split_once('/') {
            Some((provider, id))
                if self.provider.is_none() && !provider.is_empty() && !id.is_empty() =>
            {
                (Some(provider), id)
            }
            _ => (self.provider, self.id),
        }
    }
}

/// The model in force at each event of a branch
pub struct Models<'a> {
    first: Option<Model<'a>>,
    last: Option<Model<'a>>,
    by_event: HashMap<&'a str, Model<'a>>,
}

impl<'a> Models<'a> {
    /// Reads the model meta events of a whole branch, such as [`Session::path`](crate::Session::path)
    pub fn new(path: &[&'a Event]) -> Models<'a> {
        let mut models = Models {
            first: None,
            last: None,
            by_event: HashMap::new(),
        };
        for &event in path {
            if let Some(model) = models.last {
                models.by_event.insert(&event.id, model);
            }
            if let Some(model) = model(&event.body) {
                models.first.get_or_insert(model);
                models.last = Some(model);
            }
        }
        models
    }

    /// The model the latest meta event before `event` names, or else the first model the branch names
    pub fn at(&self, event: &Event) -> Option<Model<'a>> {
        self.by_event.get(event.id.as_str()).copied().or(self.first)
    }

    /// The model the last meta event of the branch names
    pub fn last(&self) -> Option<Model<'a>> {
        self.last
    }
}

/// The model this body's meta event names
pub(crate) fn model(body: &Body) -> Option<Model<'_>> {
    match body {
        Body::Meta(meta) if meta["type"] == "model" => {
            let known = |name: &&str| !name.is_empty() && *name != SYNTHETIC;
            let id = meta["model"].as_str()?;
            let id = id
                .strip_prefix(SYNTHETIC)
                .and_then(|id| id.strip_prefix('/'))
                .unwrap_or(id);
            known(&id).then(|| Model {
                provider: meta["provider"].as_str().filter(known),
                id,
            })
        }
        _ => None,
    }
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

    fn assistant(text: &str) -> Body {
        Body::Assistant(Assistant { text: text.into() })
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
            .map(|event| event.body.text().into_owned())
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

    #[test]
    fn models_follow_the_latest_switch_and_fall_back_to_the_first() {
        let bodies = [
            user("question"),
            Body::Meta(json!({"type": "model", "provider": "p", "model": "a"})),
            assistant("answer"),
            Body::Meta(json!({"type": "model", "provider": null, "model": "b"})),
            user("follow-up"),
            assistant("second answer"),
        ];
        let chained = Event::chain(bodies.into_iter().map(draft), None, Agent::Pi, "n");
        let path: Vec<&Event> = chained.iter().collect();
        let models = Models::new(&path);
        let a = Some(Model {
            provider: Some("p"),
            id: "a",
        });
        let b = Some(Model {
            provider: None,
            id: "b",
        });
        let at = [0, 2, 4, 5].map(|index| models.at(path[index]));
        assert_eq!(at, [a, a, b, b]);
        assert_eq!(models.last(), b);
    }

    #[test]
    fn a_synthetic_model_switches_nothing() {
        let bodies = [
            Body::Meta(json!({"type": "model", "model": "a"})),
            user("question"),
            Body::Meta(json!({"type": "model", "model": "<synthetic>"})),
            assistant("No response requested."),
        ];
        let chained = Event::chain(bodies.into_iter().map(draft), None, Agent::Claude, "n");
        let path: Vec<&Event> = chained.iter().collect();
        let models = Models::new(&path);
        let a = Some(Model {
            provider: None,
            id: "a",
        });
        assert_eq!(models.at(path[3]), a);
        assert_eq!(models.last(), a);
    }

    #[test]
    fn a_synthetic_provider_is_left_out() {
        let bodies = [
            json!({"type": "model", "provider": "<synthetic>", "model": "m"}),
            json!({"type": "model", "model": "<synthetic>/m"}),
            json!({"type": "model", "model": "<synthetic>/<synthetic>"}),
        ]
        .map(Body::Meta);
        let m = Some(Model {
            provider: None,
            id: "m",
        });
        assert_eq!(bodies.each_ref().map(model), [m, m, None]);
    }

    #[test]
    fn split_takes_the_provider_from_the_id_only_when_none_was_recorded() {
        let split = |provider, id| Model { provider, id }.split();
        assert_eq!(split(Some("p"), "auto/m"), (Some("p"), "auto/m"));
        assert_eq!(
            split(None, "magpie/auto/claude-opus-5-5"),
            (Some("magpie"), "auto/claude-opus-5-5")
        );
        assert_eq!(split(None, "m"), (None, "m"));
        assert_eq!(split(None, "/m"), (None, "/m"));
    }
}
