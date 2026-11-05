use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images};

// The Claude Code version written into records; newer versions read transcripts of older ones
const VERSION: &str = "2.1.209";

// Builds a Claude Code transcript as one linear chain of records
pub(super) struct TranscriptWriter<'a> {
    blobs: &'a Blobs,
    session: &'a str,
    cwd: &'a str,
    out: String,
    parent: Option<String>,
}

impl<'a> TranscriptWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs, session: &'a str, cwd: &'a str) -> TranscriptWriter<'a> {
        TranscriptWriter {
            blobs,
            session,
            cwd,
            out: String::new(),
            parent: None,
        }
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let content = self.content(&user.text, &user.images)?;
                    self.push_record(
                        &event.timestamp,
                        "user",
                        json!({"message": {"role": "user", "content": content}}),
                    );
                }
                Unit::Assistant(events) => self.push_assistant(&events),
                Unit::ToolResult(event, result) => {
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": result.call_id,
                        "content": self.content(&result.text, &result.images)?,
                        "is_error": result.is_error,
                    });
                    self.push_record(
                        &event.timestamp,
                        "user",
                        json!({"message": {"role": "user", "content": [block]}}),
                    );
                }
                Unit::Compaction(event, summary) => {
                    // Claude resumes from the boundary, which has no parent and points back only logically
                    let previous = self.parent.take();
                    let boundary = json!({
                        "subtype": "compact_boundary",
                        "content": "Conversation compacted",
                        "isMeta": false,
                        "level": "info",
                        "logicalParentUuid": previous,
                        "compactMetadata": {"trigger": "manual", "preTokens": 0},
                    });
                    self.push_record(&event.timestamp, "system", boundary);
                    let summary = json!({
                        "message": {"role": "user", "content": summary},
                        "isCompactSummary": true,
                        "isVisibleInTranscriptOnly": true,
                    });
                    self.push_record(&event.timestamp, "user", summary);
                }
            }
        }
        Ok(())
    }

    pub(super) fn push_title(&mut self, title: &str) {
        let record =
            json!({"type": "custom-title", "customTitle": title, "sessionId": self.session});
        self.push_line(&record);
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    fn push_assistant(&mut self, events: &[&Event]) {
        let mut blocks = Vec::new();
        for event in events {
            match &event.body {
                Body::Assistant(assistant) => {
                    blocks.push(json!({"type": "text", "text": assistant.text}));
                }
                Body::ToolCall(call) => {
                    blocks.push(json!({"type": "tool_use", "id": call.call_id, "name": call.name, "input": call.input}));
                }
                _ => {}
            }
        }
        let stop = if blocks.iter().any(|block| block["type"] == "tool_use") {
            "tool_use"
        } else {
            "end_turn"
        };
        let message = json!({
            "id": format!("msg_{}", identifiers::short_hex(24)),
            "type": "message",
            "role": "assistant",
            "model": "baton",
            "content": blocks,
            "stop_reason": stop,
            "stop_sequence": null,
            "usage": {"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0},
        });
        let request = format!("req_{}", identifiers::short_hex(24));
        self.push_record(
            &events[0].timestamp,
            "assistant",
            json!({"message": message, "requestId": request}),
        );
    }

    fn content(&self, text: &str, attached: &[Image]) -> Result<Value> {
        if attached.is_empty() {
            return Ok(json!(text));
        }
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        for image in attached {
            let data = images::encode(&self.blobs.get(&image.blob)?);
            blocks.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": image.mime, "data": data},
            }));
        }
        Ok(json!(blocks))
    }

    fn push_line(&mut self, value: &Value) {
        self.out.push_str(&value.to_string());
        self.out.push('\n');
    }

    fn push_record(&mut self, timestamp: &str, kind: &str, fields: Value) {
        let uuid = identifiers::uuid_v4();
        let mut record = json!({
            "parentUuid": self.parent,
            "isSidechain": false,
            "userType": "external",
            "cwd": self.cwd,
            "sessionId": self.session,
            "version": VERSION,
            "gitBranch": "",
            "type": kind,
            "uuid": uuid,
            "timestamp": timestamp,
        });
        for (key, value) in fields.as_object().expect("record fields are objects") {
            record[key] = value.clone();
        }
        self.push_line(&record);
        self.parent = Some(uuid);
    }
}
