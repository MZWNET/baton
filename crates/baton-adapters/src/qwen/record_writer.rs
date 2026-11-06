use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images, json};

// The Qwen Code version written into records; Qwen does not check it
const VERSION: &str = "0.22.1";

// Builds a Qwen Code chat as one linear chain of records
pub(super) struct RecordWriter<'a> {
    blobs: &'a Blobs,
    session: &'a str,
    cwd: &'a str,
    out: String,
    parent: Option<String>,
}

impl<'a> RecordWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs, session: &'a str, cwd: &'a str) -> RecordWriter<'a> {
        RecordWriter {
            blobs,
            session,
            cwd,
            out: String::new(),
            parent: None,
        }
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        let mut names: HashMap<&str, &str> = HashMap::new();
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let mut parts = Vec::new();
                    if !user.text.is_empty() {
                        parts.push(json!({"text": user.text}));
                    }
                    parts.extend(self.inline(&user.images)?);
                    let message = json!({"message": {"role": "user", "parts": parts}});
                    self.push(&event.timestamp, "user", "real_user", message);
                }
                Unit::Assistant(run) => {
                    let mut parts = Vec::new();
                    for event in &run {
                        match &event.body {
                            Body::Assistant(assistant) => {
                                parts.push(json!({"text": assistant.text}))
                            }
                            Body::ToolCall(call) => {
                                names.insert(&call.call_id, &call.name);
                                parts.push(json!({"functionCall": {"id": call.call_id, "name": call.name, "args": call.input}}));
                            }
                            _ => {}
                        }
                    }
                    let message = json!({"message": {"role": "model", "parts": parts}});
                    self.push(&run[0].timestamp, "assistant", "assistant_output", message);
                }
                Unit::ToolResult(event, result) => {
                    let name = names
                        .get(result.call_id.as_str())
                        .copied()
                        .unwrap_or("unknown");
                    let (key, status) = if result.is_error {
                        ("error", "error")
                    } else {
                        ("output", "success")
                    };
                    let mut response =
                        json!({"id": result.call_id, "name": name, "response": {key: result.text}});
                    // Images go inside the response, where Qwen hands them to the model
                    if !result.images.is_empty() {
                        response["parts"] = json!(self.inline(&result.images)?);
                    }
                    let record = json!({
                        "message": {"role": "user", "parts": [{"functionResponse": response}]},
                        "toolCallResult": {"callId": result.call_id, "status": status, "resultDisplay": result.text},
                    });
                    self.push(&event.timestamp, "tool_result", "tool_result", record);
                }
                // Qwen resumes a compressed chat from this history, with the reply it gives itself to a summary
                Unit::Compaction(event, summary) => {
                    let history = [
                        json!({"role": "user", "parts": [{"text": summary}]}),
                        json!({"role": "model", "parts": [{"text": "Got it. Thanks for the additional context!"}]}),
                    ];
                    let record = json!({
                        "subtype": "chat_compression",
                        "systemPayload": {"info": {"originalTokenCount": 0, "newTokenCount": 0, "compressionStatus": 1}, "compressedHistory": history},
                    });
                    self.push(&event.timestamp, "system", "system", record);
                }
            }
        }
        Ok(())
    }

    // Written last, where Qwen looks for titles and continues the chain
    pub(super) fn push_title(&mut self, timestamp: &str, title: &str) {
        let record = json!({"subtype": "custom_title", "systemPayload": {"customTitle": title, "titleSource": "manual"}});
        self.push(timestamp, "system", "system", record);
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    fn inline(&self, attached: &[Image]) -> Result<Vec<Value>> {
        let mut parts = Vec::with_capacity(attached.len());
        for image in attached {
            let data = images::encode(&self.blobs.get(&image.blob)?);
            parts.push(json!({"inlineData": {"mimeType": image.mime, "data": data}}));
        }
        Ok(parts)
    }

    fn push(&mut self, timestamp: &str, kind: &str, provenance: &str, fields: Value) {
        let uuid = identifiers::uuid_v4();
        let mut record = json!({
            "uuid": uuid,
            "parentUuid": self.parent,
            "sessionId": self.session,
            "timestamp": timestamp,
            "type": kind,
            "provenance": provenance,
            "cwd": self.cwd,
            "version": VERSION,
        });
        for (key, value) in fields.as_object().expect("record fields are objects") {
            record[key] = value.clone();
        }
        json::push_line(&mut self.out, &record);
        self.parent = Some(uuid);
    }
}
