use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Blobs, Event, Image, time};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{images, json};

// Kimi rewrites a journal of any other protocol version when it loads it
const PROTOCOL_VERSION: &str = "1.5";

// Builds a Kimi Code main agent journal from context messages, with strictly increasing times
pub(super) struct WireWriter<'a> {
    blobs: &'a Blobs,
    out: String,
    time: i64,
    // Messages in Kimi's context since the last compaction, which the next one replaces
    context: usize,
    last_prompt: Option<&'a str>,
}

impl<'a> WireWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs, created: i64) -> WireWriter<'a> {
        let mut writer = WireWriter {
            blobs,
            out: String::new(),
            time: 0,
            context: 0,
            last_prompt: None,
        };
        json::push_line(
            &mut writer.out,
            &json!({"type": "metadata", "protocol_version": PROTOCOL_VERSION, "created_at": created}),
        );
        writer
    }

    pub(super) fn push_events(&mut self, events: &[&'a Event]) -> Result<()> {
        let mut names: HashMap<&str, &str> = HashMap::new();
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let mut content = Vec::new();
                    if !user.text.is_empty() {
                        content.push(json!({"type": "text", "text": user.text}));
                    }
                    content.extend(self.image_parts(&user.images)?);
                    self.last_prompt = Some(&user.text);
                    let message = json!({"role": "user", "content": content, "toolCalls": [], "origin": {"kind": "user"}});
                    self.push_message(event, message);
                }
                // Text and calls are kept apart in a message, so a text after a call starts the next one
                Unit::Assistant(run) => {
                    for segment in unit::segments(&run) {
                        let content: Vec<Value> = segment
                            .texts
                            .iter()
                            .map(|text| json!({"type": "text", "text": text}))
                            .collect();
                        let mut calls = Vec::with_capacity(segment.calls.len());
                        for call in &segment.calls {
                            names.insert(&call.call_id, &call.name);
                            calls.push(json!({"type": "function", "id": call.call_id, "name": call.name, "arguments": call.input.to_string()}));
                        }
                        let message =
                            json!({"role": "assistant", "content": content, "toolCalls": calls});
                        self.push_message(segment.event, message);
                    }
                }
                Unit::ToolResult(event, result) => {
                    let mut content = vec![json!({"type": "text", "text": result.text})];
                    content.extend(self.image_parts(&result.images)?);
                    let mut message = json!({"role": "tool", "content": content, "toolCalls": [], "toolCallId": result.call_id});
                    if let Some(name) = names.get(result.call_id.as_str()) {
                        message["name"] = json!(name);
                    }
                    if result.is_error {
                        message["isError"] = json!(true);
                    }
                    self.push_message(event, message);
                }
                // With a legacy tail, the context Kimi resumes is the summary followed by the messages after the compacted ones
                Unit::Compaction(event, summary) => {
                    let record = json!({"type": "context.apply_compaction", "agentId": "main", "summary": summary,
                                        "compactedCount": self.context, "legacyTail": true, "tokensBefore": 0});
                    self.push_record(event, record);
                    self.context = 1;
                }
            }
        }
        Ok(())
    }

    pub(super) fn last_prompt(&self) -> Option<&'a str> {
        self.last_prompt
    }

    pub(super) fn updated(&self) -> i64 {
        self.time
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    fn image_parts(&self, attached: &[Image]) -> Result<Vec<Value>> {
        let mut parts = Vec::with_capacity(attached.len());
        for image in attached {
            let url = images::data_url(self.blobs, image)?;
            parts.push(json!({"type": "image_url", "imageUrl": {"url": url}}));
        }
        Ok(parts)
    }

    fn push_message(&mut self, event: &Event, message: Value) {
        self.push_record(
            event,
            json!({"type": "context.append_message", "agentId": "main", "message": message}),
        );
        self.context += 1;
    }

    fn push_record(&mut self, event: &Event, mut record: Value) {
        self.time = time::unix_millis(&event.timestamp).max(self.time + 1);
        record["time"] = json!(self.time);
        json::push_line(&mut self.out, &record);
    }
}
