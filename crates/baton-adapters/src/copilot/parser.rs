use std::collections::HashMap;

use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolCall, ToolResult, time,
};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Reads a Copilot event log, which Copilot replays strictly in line order
pub(super) struct EventParser<'a> {
    blobs: &'a Blobs,
    // Whether the parser continues an earlier read, which never saw the records before the cursor
    continuing: bool,
    last: Option<String>,
    assets: HashMap<String, Image>,
    // Set when a record refers to an image asset this read has not seen
    missing_asset: bool,
    compaction_start: Option<String>,
    info: Info,
}

// What one record adds to the conversation
struct Entry {
    bodies: Vec<Body>,
    anchor: Option<String>,
}

impl<'a> EventParser<'a> {
    pub(super) fn new(blobs: &'a Blobs, leaf: Option<&str>) -> EventParser<'a> {
        EventParser {
            blobs,
            continuing: leaf.is_some(),
            last: leaf.map(String::from),
            assets: HashMap::new(),
            missing_asset: false,
            compaction_start: None,
            info: Info::default(),
        }
    }

    fn entry(&mut self, record: &Value, id: &str) -> Result<Entry> {
        let data = &record["data"];
        // Subagents record their own turns into the parent's log
        let subagent = !record["agentId"].is_null() || !data["parentToolCallId"].is_null();
        let bodies = match record["type"].as_str().unwrap_or_default() {
            "session.start" => {
                self.info.cwd = json::string(&data["context"]["cwd"]);
                self.info.created = data["startTime"].as_str().and_then(time::normalize);
                model(&data["selectedModel"])
            }
            "session.binary_asset" => {
                if let (Some(asset), Some(mime), Some(bytes)) = (
                    json::string(&data["assetId"]),
                    data["mimeType"].as_str(),
                    data["data"].as_str().and_then(images::decode),
                ) {
                    self.assets
                        .insert(asset, images::store(self.blobs, mime, &bytes)?);
                }
                Vec::new()
            }
            // Messages with a source were injected by the runtime, not typed
            "user.message"
                if !subagent
                    && json::string(&data["source"]).is_none()
                    && data["isAutopilotContinuation"] != true =>
            {
                let attachments = data["attachments"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut attached = Vec::new();
                for attachment in attachments {
                    attached.extend(self.image(attachment)?);
                }
                let content = Content {
                    text: data["content"].as_str().unwrap_or_default().to_owned(),
                    images: attached,
                };
                normalize::user(content).into_iter().collect()
            }
            "assistant.message" if !subagent => {
                let mut bodies = Vec::new();
                let thinking = Thinking {
                    text: data["reasoningText"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    signature: json::string(&data["reasoningOpaque"])
                        .or_else(|| json::string(&data["encryptedContent"])),
                };
                if !thinking.text.is_empty() || thinking.signature.is_some() {
                    bodies.push(Body::Thinking(thinking));
                }
                bodies.extend(data["content"].as_str().and_then(normalize::assistant));
                let requests = data["toolRequests"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for request in requests {
                    bodies.push(Body::ToolCall(ToolCall {
                        call_id: normalize::call_id(&request["toolCallId"]),
                        name: request["name"].as_str().unwrap_or("unknown").to_owned(),
                        input: normalize::tool_input(request["arguments"].clone()),
                    }));
                }
                bodies
            }
            "tool.execution_complete" if !subagent && data["isUserRequested"] != true => {
                let result = &data["result"];
                let text = result["content"]
                    .as_str()
                    .or(data["error"]["message"].as_str())
                    .unwrap_or_default();
                let binaries = result["binaryResultsForLlm"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut attached = Vec::new();
                for binary in binaries {
                    if binary["type"] == "image" && binary["omittedReason"].is_null() {
                        attached.extend(self.image(binary)?);
                    }
                }
                vec![Body::ToolResult(ToolResult {
                    call_id: normalize::call_id(&data["toolCallId"]),
                    text: text.to_owned(),
                    images: attached,
                    is_error: data["success"] == false,
                })]
            }
            "session.compaction_start" => {
                self.compaction_start = Some(id.to_owned());
                Vec::new()
            }
            // A summary replaces the messages that existed when its compaction started, so it goes before the later ones
            "session.compaction_complete" => {
                let start = self.compaction_start.take();
                if let Some(summary) = data["summaryContent"].as_str()
                    && data["success"] == true
                {
                    let summary = summary.to_owned();
                    return Ok(Entry {
                        bodies: vec![Body::Compaction(Compaction { summary })],
                        anchor: start,
                    });
                }
                Vec::new()
            }
            "session.model_change" => model(&data["newModel"]),
            "session.context_cleared" => vec![Body::Meta(json!({"type": "reset"}))],
            _ => Vec::new(),
        };
        Ok(Entry {
            bodies,
            anchor: None,
        })
    }

    // An image attachment or tool binary, inline or referring to an earlier asset record
    fn image(&mut self, reference: &Value) -> Result<Option<Image>> {
        if let Some(asset) = reference["assetId"].as_str() {
            let image = self.assets.get(asset).cloned();
            self.missing_asset |= image.is_none();
            return Ok(image);
        }
        match (
            reference["mimeType"].as_str(),
            reference["data"].as_str().and_then(images::decode),
        ) {
            (Some(mime), Some(bytes)) if mime.starts_with("image/") => {
                images::store(self.blobs, mime, &bytes).map(Some)
            }
            _ => Ok(None),
        }
    }
}

impl Parser for EventParser<'_> {
    fn line(&mut self, _offset: u64, record: Value) -> Result<Line> {
        let Some(id) = json::string(&record["id"]) else {
            return Ok(Line::Skip);
        };
        self.missing_asset = false;
        let entry = self.entry(&record, &id)?;
        let compacted = entry
            .bodies
            .iter()
            .any(|body| matches!(body, Body::Compaction(_)));
        // An incremental read never saw the assets and the compaction start before the cursor
        if self.continuing && (self.missing_asset || compacted) {
            return Ok(Line::Restart);
        }
        let fallback = self.info.created.clone().unwrap_or_else(time::now);
        let timestamp = json::timestamp(&record["timestamp"], &fallback);
        let mut drafts = Vec::with_capacity(entry.bodies.len());
        for body in entry.bodies {
            drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: Some(id.clone()),
                body,
            });
        }
        // The file order is the chain, which also holds where Copilot rotated or repaired the stored parent links
        let parent = self.last.replace(id.clone());
        Ok(Line::Node(Node {
            id,
            parent,
            drafts,
            anchor: entry.anchor,
        }))
    }

    fn finish(self) -> Info {
        self.info
    }
}

fn model(value: &Value) -> Vec<Body> {
    match value.as_str() {
        Some(model) => vec![Body::Meta(json!({"type": "model", "model": model}))],
        None => Vec::new(),
    }
}
