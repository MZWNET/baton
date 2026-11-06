use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Blobs, Event, Image, time};
use chrono::{DateTime, Local, NaiveDateTime, TimeDelta};
use serde_json::{Value, json};

use crate::support::unit::{self, Segment, Unit};
use crate::support::{identifiers, images};

// OpenHands rebuilds the model's system message from the first event, so it is what the resumed model reads instead of OpenHands' own prompt
const SYSTEM_PROMPT: &str = "This conversation was imported from another coding agent. Its earlier tool calls are history and were not run here.";

// Builds the event documents of an OpenHands conversation; events reject unknown fields, so everything maps onto native ones
pub(super) struct EventWriter<'a> {
    blobs: &'a Blobs,
    events: Vec<Value>,
    clock: Option<NaiveDateTime>,
    // Every event after the system prompt, which a summary makes the model forget
    forgotten: Vec<String>,
    actions: HashMap<&'a str, (String, &'a str)>,
}

impl<'a> EventWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs) -> EventWriter<'a> {
        EventWriter {
            blobs,
            events: Vec::new(),
            clock: None,
            forgotten: Vec::new(),
            actions: HashMap::new(),
        }
    }

    pub(super) fn push_events(&mut self, created: &str, events: &[&'a Event]) -> Result<()> {
        // OpenHands refuses a conversation whose first messages come without a system prompt
        let prompt = json!({"system_prompt": {"type": "text", "text": SYSTEM_PROMPT}, "tools": []});
        self.push(created, "SystemPromptEvent", "agent", prompt);
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let text = (!user.text.is_empty()).then_some(user.text.as_str());
                    let content = self.content(text, &user.images)?;
                    let message = json!({"llm_message": {"role": "user", "content": content}});
                    self.push_forgettable(&event.timestamp, "MessageEvent", "user", message);
                }
                Unit::Assistant(run) => {
                    for segment in unit::segments(&run) {
                        self.push_segment(segment);
                    }
                }
                Unit::ToolResult(event, result) => {
                    // A result whose call was not written refers to no action, which keeps it from the model
                    let (action, name) = self
                        .actions
                        .get(result.call_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| (identifiers::uuid_v4(), "unknown"));
                    let content = self.content(Some(&result.text), &result.images)?;
                    let observation = json!({"tool_name": name, "tool_call_id": result.call_id, "action_id": action,
                                             "observation": {"content": content, "is_error": result.is_error, "command": name, "kind": "TerminalObservation"}});
                    self.push_forgettable(
                        &event.timestamp,
                        "ObservationEvent",
                        "environment",
                        observation,
                    );
                }
                Unit::Compaction(event, summary) => {
                    // The summary goes right after the system prompt, in place of every event before it
                    let condensation = json!({"forgotten_event_ids": self.forgotten, "summary": summary, "summary_offset": 1,
                                              "llm_response_id": identifiers::uuid_v4()});
                    let id = self.push(
                        &event.timestamp,
                        "Condensation",
                        "environment",
                        condensation,
                    );
                    self.forgotten.push(format!("{id}-summary"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Vec<Value> {
        self.events
    }

    // Calls of one segment share the response they came in, and only the first carries the text before them
    fn push_segment(&mut self, segment: Segment<'a>) {
        if segment.calls.is_empty() {
            let content: Vec<Value> = segment
                .texts
                .iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect();
            let message = json!({"llm_message": {"role": "assistant", "content": content}, "llm_response_id": identifiers::uuid_v4()});
            self.push_forgettable(&segment.event.timestamp, "MessageEvent", "agent", message);
            return;
        }
        let response = identifiers::uuid_v4();
        let mut thought: Vec<Value> = segment
            .texts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect();
        for call in segment.calls {
            // A pending action would run on resume, so the action itself stays out
            let action = json!({
                "thought": thought, "tool_name": call.name, "tool_call_id": call.call_id, "llm_response_id": response,
                "tool_call": {"id": call.call_id, "name": call.name, "arguments": call.input.to_string(), "origin": "completion"},
            });
            thought.clear();
            let id =
                self.push_forgettable(&segment.event.timestamp, "ActionEvent", "agent", action);
            self.actions.insert(&call.call_id, (id, &call.name));
        }
    }

    fn content(&self, text: Option<&str>, attached: &[Image]) -> Result<Vec<Value>> {
        let mut content = Vec::new();
        if let Some(text) = text {
            content.push(json!({"type": "text", "text": text}));
        }
        for image in attached {
            content.push(
                json!({"type": "image", "image_urls": [images::data_url(self.blobs, image)?]}),
            );
        }
        Ok(content)
    }

    fn push_forgettable(
        &mut self,
        timestamp: &str,
        kind: &str,
        source: &str,
        fields: Value,
    ) -> String {
        let id = self.push(timestamp, kind, source, fields);
        self.forgotten.push(id.clone());
        id
    }

    // Timestamps are naive local times that never repeat, which the conversation list sorts by
    fn push(&mut self, timestamp: &str, kind: &str, source: &str, mut fields: Value) -> String {
        let local = DateTime::from_timestamp_millis(time::unix_millis(timestamp))
            .unwrap_or_default()
            .with_timezone(&Local)
            .naive_local();
        let stamp = match self.clock {
            Some(last) if local <= last => last + TimeDelta::microseconds(1),
            _ => local,
        };
        self.clock = Some(stamp);
        let id = identifiers::uuid_v4();
        fields["id"] = json!(id);
        fields["timestamp"] = json!(stamp.format("%Y-%m-%dT%H:%M:%S%.6f").to_string());
        fields["source"] = json!(source);
        fields["kind"] = json!(kind);
        self.events.push(fields);
        id
    }
}
