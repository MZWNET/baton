use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image, time};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{files, identifiers, images, json};

const TITLE_SLOT: usize = 256;

// Builds a pi or Oh My Pi journal: a linear chain of entries with random 8 digit hex ids
pub(super) struct JournalWriter<'a> {
    blobs: &'a Blobs,
    // Oh My Pi references images stored in this directory by hash; pi inlines them
    blob_dir: Option<PathBuf>,
    out: String,
    parent: Option<String>,
    ids: HashSet<String>,
    tool_names: HashMap<String, String>,
}

impl<'a> JournalWriter<'a> {
    pub(super) fn new(blobs: &'a Blobs, blob_dir: Option<PathBuf>) -> JournalWriter<'a> {
        JournalWriter {
            blobs,
            blob_dir,
            out: String::new(),
            parent: None,
            ids: HashSet::new(),
            tool_names: HashMap::new(),
        }
    }

    pub(super) fn push_raw(&mut self, text: &str) {
        self.out.push_str(text);
    }

    pub(super) fn push_line(&mut self, value: &Value) {
        json::push_line(&mut self.out, value);
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let content = if user.images.is_empty() {
                        json!(user.text)
                    } else {
                        json!(self.content(&user.text, &user.images)?)
                    };
                    self.push_message(event, json!({"role": "user", "content": content}));
                }
                Unit::Assistant(events) => self.push_assistant(&events),
                Unit::ToolResult(event, result) => {
                    let name = self
                        .tool_names
                        .get(&result.call_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    let message = json!({
                        "role": "toolResult",
                        "toolCallId": result.call_id,
                        "toolName": name,
                        "content": self.content(&result.text, &result.images)?,
                        "isError": result.is_error,
                    });
                    self.push_message(event, message);
                }
                Unit::Compaction(event, summary) => {
                    // Keeping no entries before the summary matches how every other format resumes after compaction
                    let id = self.next_id();
                    let entry = json!({"type": "compaction", "summary": summary, "firstKeptEntryId": id, "tokensBefore": 0});
                    self.push_entry(id, &event.timestamp, entry);
                }
            }
        }
        Ok(())
    }

    pub(super) fn push_session_info(&mut self, title: &str, timestamp: &str) {
        let id = self.next_id();
        self.push_entry(
            id,
            timestamp,
            json!({"type": "session_info", "name": title}),
        );
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
                    self.tool_names
                        .insert(call.call_id.clone(), call.name.clone());
                    blocks.push(json!({"type": "toolCall", "id": call.call_id, "name": call.name, "arguments": call.input}));
                }
                _ => {}
            }
        }
        let stop = if blocks.iter().any(|block| block["type"] == "toolCall") {
            "toolUse"
        } else {
            "stop"
        };
        // Oh My Pi will not open a session whose model it cannot restore but ignores `<synthetic>`, and pi falls back from unknown models, so both use the configured model
        let message = json!({
            "role": "assistant",
            "content": blocks,
            "api": "baton",
            "provider": "baton",
            "model": "<synthetic>",
            "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": stop,
        });
        self.push_message(events[0], message);
    }

    fn content(&self, text: &str, images: &[Image]) -> Result<Vec<Value>> {
        let mut blocks = Vec::new();
        if !text.is_empty() || images.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        for image in images {
            blocks.push(self.image_block(image)?);
        }
        Ok(blocks)
    }

    fn image_block(&self, image: &Image) -> Result<Value> {
        let data = match &self.blob_dir {
            Some(blob_dir) => {
                let path = blob_dir.join(&image.blob);
                if !path.exists() {
                    files::publish(&path, &self.blobs.get(&image.blob)?)?;
                }
                format!("blob:sha256:{}", image.blob)
            }
            None => images::encode(&self.blobs.get(&image.blob)?),
        };
        Ok(json!({"type": "image", "data": data, "mimeType": image.mime}))
    }

    fn next_id(&mut self) -> String {
        loop {
            let id = identifiers::short_hex(8);
            if self.ids.insert(id.clone()) {
                return id;
            }
        }
    }

    fn push_entry(&mut self, id: String, timestamp: &str, mut entry: Value) {
        entry["id"] = json!(id);
        entry["parentId"] = json!(self.parent);
        entry["timestamp"] = json!(timestamp);
        self.push_line(&entry);
        self.parent = Some(id);
    }

    fn push_message(&mut self, event: &Event, mut message: Value) {
        message["timestamp"] = json!(time::unix_millis(&event.timestamp));
        let id = self.next_id();
        self.push_entry(
            id,
            &event.timestamp,
            json!({"type": "message", "message": message}),
        );
    }
}

// Oh My Pi's first line: a title record padded to exactly 256 bytes so the title can be rewritten in place
pub(super) fn title_slot(title: &str, updated: &str) -> String {
    let record = |title: &str, pad: usize| {
        let mut value = json!({"type": "title", "v": 1, "title": title, "updatedAt": updated, "pad": " ".repeat(pad)});
        if !title.is_empty() {
            value["source"] = json!("user");
        }
        format!("{value}\n")
    };
    let mut chars: Vec<char> = title.chars().collect();
    while record(&chars.iter().collect::<String>(), 0).len() > TITLE_SLOT {
        chars.pop();
    }
    let title: String = chars.into_iter().collect();
    record(&title, TITLE_SLOT - record(&title, 0).len())
}
