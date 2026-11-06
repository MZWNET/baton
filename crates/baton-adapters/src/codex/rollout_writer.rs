use std::path::Path;

use anyhow::Result;
use baton_core::{Blobs, Body, Event};
use serde_json::{Value, json};

use crate::support::{images, json};

// The Codex version written into session_meta; newer versions resume rollouts of older ones
const CLI_VERSION: &str = "0.144.4";

// Builds a legacy rollout, which Codex replays from its response items and shows from its event messages
pub(super) struct RolloutWriter<'a> {
    blobs: &'a Blobs,
    out: String,
}

impl<'a> RolloutWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs) -> RolloutWriter<'a> {
        RolloutWriter {
            blobs,
            out: String::new(),
        }
    }

    pub(super) fn push_meta(&mut self, id: &str, created: &str, cwd: &Path, provider: &str) {
        let meta = json!({
            "id": id,
            "timestamp": created,
            "cwd": cwd.to_string_lossy(),
            "originator": "baton",
            "cli_version": CLI_VERSION,
            "source": "cli",
            "model_provider": provider,
        });
        self.push_record(created, "session_meta", meta);
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        for event in events {
            let timestamp = event.timestamp.as_str();
            match &event.body {
                Body::User(user) => {
                    let mut urls = Vec::with_capacity(user.images.len());
                    for image in &user.images {
                        urls.push(images::data_url(self.blobs, image)?);
                    }
                    let mut content = Vec::new();
                    if !user.text.is_empty() {
                        content.push(json!({"type": "input_text", "text": user.text}));
                    }
                    for url in &urls {
                        content.push(json!({"type": "input_image", "image_url": url}));
                    }
                    self.push_record(
                        timestamp,
                        "event_msg",
                        json!({"type": "user_message", "message": user.text, "images": urls}),
                    );
                    self.push_record(
                        timestamp,
                        "response_item",
                        json!({"type": "message", "role": "user", "content": content}),
                    );
                }
                Body::Assistant(assistant) => {
                    self.push_record(
                        timestamp,
                        "event_msg",
                        json!({"type": "agent_message", "message": assistant.text}),
                    );
                    let content = [json!({"type": "output_text", "text": assistant.text})];
                    self.push_record(
                        timestamp,
                        "response_item",
                        json!({"type": "message", "role": "assistant", "content": content}),
                    );
                }
                Body::ToolCall(call) => {
                    let item = json!({"type": "function_call", "name": call.name, "arguments": call.input.to_string(), "call_id": call.call_id});
                    self.push_record(timestamp, "response_item", item);
                }
                Body::ToolResult(result) => {
                    let output = if result.images.is_empty() {
                        json!(result.text)
                    } else {
                        let mut items = vec![json!({"type": "input_text", "text": result.text})];
                        for image in &result.images {
                            let url = images::data_url(self.blobs, image)?;
                            items.push(json!({"type": "input_image", "image_url": url}));
                        }
                        json!(items)
                    };
                    let item = json!({"type": "function_call_output", "call_id": result.call_id, "output": output});
                    self.push_record(timestamp, "response_item", item);
                }
                Body::Compaction(compaction) => {
                    self.push_record(
                        timestamp,
                        "compacted",
                        json!({"message": compaction.summary}),
                    );
                }
                Body::Thinking(_) | Body::Meta(_) => {}
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    fn push_record(&mut self, timestamp: &str, kind: &str, payload: Value) {
        let record = json!({"timestamp": timestamp, "type": kind, "payload": payload});
        json::push_line(&mut self.out, &record);
    }
}
