use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Blobs, Event, Image};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images, json};

// The Copilot version written into session.start; newer versions resume sessions of older ones
const COPILOT_VERSION: &str = "1.0.70";

// Builds a Copilot event log: one record per line, each the parent of the next, with nondecreasing timestamps
pub(super) struct EventWriter<'a> {
    blobs: &'a Blobs,
    out: String,
    parent: Option<String>,
    timestamp: String,
    assets: HashMap<String, usize>,
}

impl<'a> EventWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs) -> EventWriter<'a> {
        EventWriter {
            blobs,
            out: String::new(),
            parent: None,
            timestamp: String::new(),
            assets: HashMap::new(),
        }
    }

    pub(super) fn push_start(&mut self, id: &str, created: &str, cwd: &str) {
        let data = json!({"sessionId": id, "version": 1, "producer": "baton", "copilotVersion": COPILOT_VERSION,
                          "startTime": created, "context": {"cwd": cwd}, "alreadyInUse": false, "remoteSteerable": false});
        self.push(created, "session.start", data);
    }

    // Copilot moves results next to their calls before it calls the model, so the log keeps the exact order
    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let mut attachments = Vec::with_capacity(user.images.len());
                    for (index, image) in user.images.iter().enumerate() {
                        let mut reference = self.asset(&event.timestamp, image)?;
                        reference["type"] = json!("blob");
                        reference["displayName"] = json!(format!(
                            "image-{}.{}",
                            index + 1,
                            images::extension(&image.mime)
                        ));
                        attachments.push(reference);
                    }
                    let mut data = json!({"content": user.text});
                    if !attachments.is_empty() {
                        data["attachments"] = json!(attachments);
                    }
                    self.push(&event.timestamp, "user.message", data);
                }
                Unit::Assistant(run) => {
                    for reply in unit::replies(&run) {
                        let mut data =
                            json!({"messageId": identifiers::uuid_v4(), "content": reply.text});
                        // Untyped requests whose ids start with `custom_` would be taken for custom grammar tools
                        let requests: Vec<Value> = reply
                            .calls
                            .iter()
                            .map(|call| json!({"toolCallId": call.call_id, "name": call.name, "arguments": call.input, "type": "function"}))
                            .collect();
                        if !requests.is_empty() {
                            data["toolRequests"] = json!(requests);
                        }
                        self.push(&reply.event.timestamp, "assistant.message", data);
                        for call in &reply.calls {
                            let start = json!({"toolCallId": call.call_id, "toolName": call.name, "arguments": call.input});
                            self.push(&reply.event.timestamp, "tool.execution_start", start);
                        }
                    }
                }
                Unit::ToolResult(event, result) => {
                    let mut binaries = Vec::with_capacity(result.images.len());
                    for image in &result.images {
                        let mut reference = self.asset(&event.timestamp, image)?;
                        reference["type"] = json!("image");
                        binaries.push(reference);
                    }
                    let mut data = json!({"toolCallId": result.call_id, "success": !result.is_error,
                                          "result": {"content": result.text, "binaryResultsForLlm": binaries}});
                    if result.is_error {
                        data["error"] = json!({"message": result.text});
                    }
                    self.push(&event.timestamp, "tool.execution_complete", data);
                }
                Unit::Compaction(event, summary) => {
                    let data = json!({"success": true, "summaryContent": summary});
                    self.push(&event.timestamp, "session.compaction_complete", data);
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish(self) -> String {
        self.out
    }

    // Writes the asset record before the first record that refers to it, and returns the reference
    fn asset(&mut self, timestamp: &str, image: &Image) -> Result<Value> {
        let asset = format!("sha256:{}", image.blob);
        let length = if let Some(&length) = self.assets.get(&asset) {
            length
        } else {
            let bytes = self.blobs.get(&image.blob)?;
            let length = bytes.len();
            let data = json!({"assetId": asset, "type": "image", "mimeType": image.mime,
                              "byteLength": length, "data": images::encode(&bytes)});
            self.push(timestamp, "session.binary_asset", data);
            self.assets.insert(asset.clone(), length);
            length
        };

        Ok(json!({"assetId": asset, "mimeType": image.mime, "byteLength": length}))
    }

    fn push(&mut self, timestamp: &str, kind: &str, data: Value) {
        if timestamp > self.timestamp.as_str() {
            self.timestamp = timestamp.to_owned();
        }
        let id = identifiers::uuid_v4();
        let record = json!({"type": kind, "data": data, "id": id, "timestamp": self.timestamp, "parentId": self.parent});
        json::push_line(&mut self.out, &record);
        self.parent = Some(id);
    }
}
