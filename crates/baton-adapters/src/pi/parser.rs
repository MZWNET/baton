use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolCall, ToolResult, time,
};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Reads the entries of a pi or Oh My Pi journal
pub(super) struct JournalParser<'a> {
    blobs: &'a Blobs,
    blob_dir: Option<PathBuf>,
    // Version 1 journals have no ids, so their entries chain onto the previous one
    last: Option<String>,
    info: Info,
    slot_title: Option<String>,
    title: Option<String>,
}

// What one entry adds to the conversation
struct Entry {
    bodies: Vec<Body>,
    anchor: Option<String>,
}

impl<'a> JournalParser<'a> {
    pub(super) fn new(
        blobs: &'a Blobs,
        blob_dir: Option<PathBuf>,
        leaf: Option<&str>,
    ) -> JournalParser<'a> {
        JournalParser {
            blobs,
            blob_dir,
            last: leaf.map(String::from),
            info: Info::default(),
            slot_title: None,
            title: None,
        }
    }

    fn entry(&mut self, value: &Value, id: &str) -> Result<Entry> {
        let bodies = match value["type"].as_str().unwrap_or_default() {
            "message" => self.message(&value["message"])?,
            "compaction" => {
                // Pi keeps the entries from firstKeptEntryId on, so the summary has to come before them
                let anchor = json::string(&value["firstKeptEntryId"]).filter(|anchor| anchor != id);
                let summary = json::string(&value["summary"]);
                let bodies = summary
                    .map(|summary| Body::Compaction(Compaction { summary }))
                    .into_iter()
                    .collect();
                return Ok(Entry { bodies, anchor });
            }
            "branch_summary" => {
                meta(json!({"type": "branch_summary", "summary": value["summary"]}))
            }
            "custom_message" => meta(json!({
                "type": "custom_message",
                "custom_type": value["customType"],
                "text": content_text(&value["content"]),
            })),
            "model_change" => match value["model"].as_str() {
                Some(model) => meta(json!({"type": "model", "model": model})),
                None => meta(
                    json!({"type": "model", "provider": value["provider"], "model": value["modelId"]}),
                ),
            },
            "thinking_level_change" => {
                meta(json!({"type": "thinking_level", "level": value["thinkingLevel"]}))
            }
            "reset_boundary" => meta(json!({"type": "reset"})),
            "session_info" => {
                self.title = json::string(&value["name"]).or(self.title.take());
                Vec::new()
            }
            "title_change" => {
                self.title = json::string(&value["title"]).or(self.title.take());
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(Entry {
            bodies,
            anchor: None,
        })
    }

    fn message(&self, message: &Value) -> Result<Vec<Body>> {
        let content = &message["content"];
        let bodies = match message["role"].as_str().unwrap_or_default() {
            "user" => normalize::user(self.content(content)?)
                .into_iter()
                .collect(),
            "assistant" => {
                let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
                let mut bodies = Vec::new();
                for block in blocks {
                    bodies.extend(assistant_block(block)?);
                }
                bodies
            }
            "toolResult" => {
                let content = self.content(content)?;
                vec![Body::ToolResult(ToolResult {
                    call_id: normalize::call_id(&message["toolCallId"])?,
                    text: content.text,
                    images: content.images,
                    is_error: message["isError"] == true,
                })]
            }
            "bashExecution" => meta(
                json!({"type": "bash", "command": message["command"], "output": message["output"]}),
            ),
            // System messages hold pi's own prompt, which every agent rebuilds itself
            "system" => Vec::new(),
            role => meta(json!({"type": role, "text": content_text(content)})),
        };
        Ok(bodies)
    }

    fn content(&self, content: &Value) -> Result<Content> {
        let Some(blocks) = content.as_array() else {
            return Ok(Content {
                text: content.as_str().unwrap_or_default().to_owned(),
                images: Vec::new(),
            });
        };
        let mut images = Vec::new();
        for block in blocks {
            if block["type"] == "image"
                && let Some(image) = self.image(block)?
            {
                images.push(image);
            }
        }
        Ok(Content {
            text: json::texts(blocks, &["text"]),
            images,
        })
    }

    fn image(&self, block: &Value) -> Result<Option<Image>> {
        let (Some(data), Some(mime)) = (block["data"].as_str(), block["mimeType"].as_str()) else {
            return Ok(None);
        };
        let bytes = match data.strip_prefix("blob:sha256:") {
            Some(digest) => self
                .blob_dir
                .as_ref()
                .and_then(|blob_dir| fs::read(blob_dir.join(digest)).ok()),
            None => images::decode(data),
        };
        match bytes {
            Some(bytes) => images::store(self.blobs, mime, &bytes).map(Some),
            None => Ok(None),
        }
    }
}

impl Parser for JournalParser<'_> {
    fn line(&mut self, offset: u64, value: Value) -> Result<Line> {
        match value["type"].as_str() {
            Some("title") => {
                self.slot_title = json::string(&value["title"]);
                return Ok(Line::Skip);
            }
            Some("session") => {
                self.info.cwd = json::string(&value["cwd"]);
                self.info.created = value["timestamp"].as_str().and_then(time::normalize);
                return Ok(Line::Skip);
            }
            _ => {}
        }
        let (id, parent) = match json::string(&value["id"]) {
            Some(id) => (id, json::string(&value["parentId"])),
            None => (format!("@{offset}"), self.last.clone()),
        };
        self.last = Some(id.clone());
        let fallback = self.info.created.clone().unwrap_or_else(time::now);
        let timestamp = json::timestamp(&value["timestamp"], &fallback);
        let entry = self.entry(&value, &id)?;
        let mut drafts = Vec::with_capacity(entry.bodies.len());
        for body in entry.bodies {
            drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: Some(id.clone()),
                body,
            });
        }
        Ok(Line::Node(Node {
            id,
            parent,
            drafts,
            anchor: entry.anchor,
        }))
    }

    // The title slot is what Oh My Pi shows, so it wins over title entries
    fn finish(self) -> Info {
        Info {
            title: self.slot_title.or(self.title),
            ..self.info
        }
    }
}

fn assistant_block(block: &Value) -> Result<Option<Body>> {
    let body = match block["type"].as_str().unwrap_or_default() {
        "text" => block["text"].as_str().and_then(normalize::assistant),
        "thinking" => {
            let text = block["thinking"].as_str().unwrap_or_default().to_owned();
            let signature = json::string(&block["thinkingSignature"]);
            if text.is_empty() && signature.is_none() {
                return Ok(None);
            }
            Some(Body::Thinking(Thinking { text, signature }))
        }
        "toolCall" => Some(Body::ToolCall(ToolCall {
            call_id: normalize::call_id(&block["id"])?,
            name: block["name"].as_str().unwrap_or("unknown").to_owned(),
            input: normalize::tool_input(block["arguments"].clone()),
        })),
        _ => None,
    };
    Ok(body)
}

fn meta(value: Value) -> Vec<Body> {
    vec![Body::Meta(value)]
}

fn content_text(content: &Value) -> String {
    match content {
        Value::Array(blocks) => json::texts(blocks, &["text"]),
        other => other.as_str().unwrap_or_default().to_owned(),
    }
}
