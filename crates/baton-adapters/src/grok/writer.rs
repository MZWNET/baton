use std::collections::HashSet;

use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image, time};
use serde_json::{Value, json};

use crate::grok::parser::SUMMARY_PREFIX;
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images, json};

// Builds `updates.jsonl` as ACP updates, giving each message its own id so that adjacent messages read back apart
pub(super) struct UpdateWriter<'a> {
    blobs: &'a Blobs,
    session: &'a str,
    out: String,
    lines: usize,
    millis: i64,
}

impl<'a> UpdateWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs, session: &'a str) -> UpdateWriter<'a> {
        UpdateWriter {
            blobs,
            session,
            out: String::new(),
            lines: 0,
            millis: 0,
        }
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        let mut calls = HashSet::new();
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let message = identifiers::uuid_v4();
                    if !user.text.is_empty() {
                        let content = json!({"type": "text", "text": user.text});
                        self.push(event, json!({"sessionUpdate": "user_message_chunk", "messageId": message, "content": content}));
                    }
                    for image in &user.images {
                        let content = self.image(image)?;
                        self.push(event, json!({"sessionUpdate": "user_message_chunk", "messageId": message, "content": content}));
                    }
                }
                Unit::Assistant(run) => {
                    for event in run {
                        match &event.body {
                            Body::Assistant(assistant) => {
                                let content = json!({"type": "text", "text": assistant.text});
                                let message = identifiers::uuid_v4();
                                self.push(event, json!({"sessionUpdate": "agent_message_chunk", "messageId": message, "content": content}));
                            }
                            Body::ToolCall(call) => {
                                calls.insert(call.call_id.as_str());
                                self.push(event, json!({"sessionUpdate": "tool_call", "toolCallId": call.call_id, "title": call.name,
                                                        "kind": "other", "status": "pending", "rawInput": call.input}));
                            }
                            _ => {}
                        }
                    }
                }
                Unit::ToolResult(event, result) => {
                    let synthetic = !calls.contains(result.call_id.as_str());
                    let id = if synthetic {
                        let id = format!("baton-synthetic-{}", identifiers::uuid_v4());
                        self.push(event, json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "unknown", "kind": "other", "status": "pending", "rawInput": {}, "_meta": {"baton": {"synthetic": true, "call_id": result.call_id}}}));
                        id
                    } else {
                        result.call_id.clone()
                    };
                    // A text block, even an empty one, keeps the reader from falling back to the raw output
                    let mut content = vec![
                        json!({"type": "content", "content": {"type": "text", "text": result.text}}),
                    ];
                    for image in &result.images {
                        let mut block = self.image(image)?;
                        block
                            .as_object_mut()
                            .expect("image blocks are objects")
                            .remove("uri");
                        content.push(json!({"type": "content", "content": block}));
                    }
                    let status = if result.is_error {
                        "failed"
                    } else {
                        "completed"
                    };
                    let mut update = json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "content": content});
                    if synthetic {
                        update["_meta"] =
                            json!({"baton": {"synthetic": true, "call_id": result.call_id}});
                    }
                    self.push(event, update);
                }
                Unit::Compaction(event, summary) => {
                    let content =
                        json!({"type": "text", "text": format!("{SUMMARY_PREFIX}{summary}")});
                    self.push(event, json!({"sessionUpdate": "user_message_chunk", "messageId": identifiers::uuid_v4(),
                                            "content": content, "_meta": {"baton": "compaction"}}));
                }
            }
        }
        Ok(())
    }

    pub(super) fn lines(&self) -> usize {
        self.lines
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    // Grok rebuilds user images from the URI, while ACP requires the data and the media type
    fn image(&self, image: &Image) -> Result<Value> {
        let uri = images::data_url(self.blobs, image)?;
        let data = uri.split_once(',').expect("data URLs have a payload").1;
        Ok(json!({"type": "image", "data": data, "mimeType": image.mime, "uri": uri}))
    }

    fn push(&mut self, event: &Event, update: Value) {
        self.millis = self.millis.max(time::unix_millis(&event.timestamp));
        let line = json!({
            "timestamp": self.millis.div_euclid(1000),
            "method": "session/update",
            "params": {"sessionId": self.session, "update": update, "_meta": {"agentTimestampMs": self.millis}},
        });
        json::push_line(&mut self.out, &line);
        self.lines += 1;
    }
}
