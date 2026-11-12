use std::mem;

use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolCall, ToolResult, time,
};
use serde_json::{Value, json};

use crate::support::model::LEGACY_PLACEHOLDER;
use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Reads the records of a Claude Code transcript
pub(super) struct TranscriptParser<'a> {
    blobs: &'a Blobs,
    info: Info,
    custom_title: Option<String>,
    ai_title: Option<String>,
    model: Option<String>,
}

impl<'a> TranscriptParser<'a> {
    pub(super) fn new(blobs: &'a Blobs) -> TranscriptParser<'a> {
        TranscriptParser {
            blobs,
            info: Info::default(),
            custom_title: None,
            ai_title: None,
            model: None,
        }
    }

    fn bodies(&mut self, record: &Value) -> Result<Vec<Body>> {
        let content = &record["message"]["content"];
        match record["type"].as_str().unwrap_or_default() {
            "user" if record["isCompactSummary"] == true => {
                let summary = text_of(content);
                Ok(summary
                    .map(|summary| Body::Compaction(Compaction { summary }))
                    .into_iter()
                    .collect())
            }
            "user" if record["isMeta"] != true => self.user(content),
            // Claude Code writes `<synthetic>` placeholders such as API errors and "No response requested."
            "assistant"
                if record["isApiErrorMessage"] != true
                    && record["message"]["model"] != "<synthetic>" =>
            {
                let mut bodies = Vec::new();
                match content {
                    Value::Array(blocks) => {
                        for block in blocks {
                            bodies.extend(assistant_block(block)?);
                        }
                    }
                    other => bodies.extend(other.as_str().and_then(normalize::assistant)),
                }
                if !bodies.is_empty()
                    && let Some(model) = record["message"]["model"]
                        .as_str()
                        .filter(|model| !model.is_empty() && *model != LEGACY_PLACEHOLDER)
                    && self.model.as_deref() != Some(model)
                {
                    bodies.insert(0, Body::Meta(json!({"type": "model", "model": model})));
                    self.model = Some(model.to_owned());
                }
                Ok(bodies)
            }
            _ => Ok(Vec::new()),
        }
    }

    // A user record can mix tool results with text, so text before a result becomes its own message
    fn user(&self, content: &Value) -> Result<Vec<Body>> {
        let Some(blocks) = content.as_array() else {
            let text = content.as_str().unwrap_or_default().to_owned();
            let content = Content {
                text,
                images: Vec::new(),
            };
            return Ok(normalize::user(content).into_iter().collect());
        };
        let mut bodies = Vec::new();
        let mut texts = Vec::new();
        let mut attached = Vec::new();
        for block in blocks {
            match block["type"].as_str().unwrap_or_default() {
                "text" => texts.extend(block["text"].as_str().map(String::from)),
                "image" => attached.extend(self.image(&block["source"])?),
                "tool_result" => {
                    let before = Content {
                        text: texts.join("\n"),
                        images: mem::take(&mut attached),
                    };
                    bodies.extend(normalize::user(before));
                    texts.clear();
                    let result = self.result_content(&block["content"])?;
                    bodies.push(Body::ToolResult(ToolResult {
                        call_id: normalize::call_id(&block["tool_use_id"])?,
                        text: result.text,
                        images: result.images,
                        is_error: block["is_error"] == true,
                    }));
                }
                _ => {}
            }
        }
        let after = Content {
            text: texts.join("\n"),
            images: attached,
        };
        bodies.extend(normalize::user(after));
        Ok(bodies)
    }

    fn result_content(&self, content: &Value) -> Result<Content> {
        let Some(blocks) = content.as_array() else {
            return Ok(Content {
                text: content.as_str().unwrap_or_default().to_owned(),
                images: Vec::new(),
            });
        };
        let mut attached = Vec::new();
        for block in blocks {
            if block["type"] == "image" {
                attached.extend(self.image(&block["source"])?);
            }
        }
        Ok(Content {
            text: json::texts(blocks, &["text"]),
            images: attached,
        })
    }

    fn image(&self, source: &Value) -> Result<Option<Image>> {
        let (Some("base64"), Some(mime), Some(data)) = (
            source["type"].as_str(),
            source["media_type"].as_str(),
            source["data"].as_str(),
        ) else {
            return Ok(None);
        };
        match images::decode(data) {
            Some(bytes) => images::store(self.blobs, mime, &bytes).map(Some),
            None => Ok(None),
        }
    }
}

impl Parser for TranscriptParser<'_> {
    fn line(&mut self, _offset: u64, record: Value) -> Result<Line> {
        if record["isSidechain"] == true {
            return Ok(Line::Skip);
        }
        match record["type"].as_str().unwrap_or_default() {
            "custom-title" => {
                self.custom_title =
                    json::string(&record["customTitle"]).or(self.custom_title.take());
            }
            "ai-title" => {
                self.ai_title = json::string(&record["aiTitle"]).or(self.ai_title.take());
            }
            // Claude Code 1.x titled sessions with summary records
            "summary" if self.ai_title.is_none() => {
                self.ai_title = json::string(&record["summary"]);
            }
            _ => {}
        }
        let Some(id) = json::string(&record["uuid"]) else {
            return Ok(Line::Skip);
        };
        if self.info.cwd.is_none() {
            self.info.cwd = json::string(&record["cwd"]);
        }
        let fallback = self.info.created.clone().unwrap_or_else(time::now);
        let timestamp = json::timestamp(&record["timestamp"], &fallback);
        if self.info.created.is_none() {
            self.info.created = Some(timestamp.clone());
        }
        // A compaction boundary has no parent; it points back through logicalParentUuid instead
        let boundary = record["type"] == "system" && record["subtype"] == "compact_boundary";
        let parent = match json::string(&record["parentUuid"]) {
            Some(parent) => Some(parent),
            None if boundary => json::string(&record["logicalParentUuid"]),
            None => None,
        };
        let mut drafts = Vec::new();
        for body in self.bodies(&record)? {
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
            anchor: None,
        }))
    }

    fn finish(self) -> Info {
        Info {
            title: self.custom_title.or(self.ai_title),
            ..self.info
        }
    }
}

fn assistant_block(block: &Value) -> Result<Option<Body>> {
    let body = match block["type"].as_str().unwrap_or_default() {
        "text" => block["text"].as_str().and_then(normalize::assistant),
        "thinking" => Some(Body::Thinking(Thinking {
            text: block["thinking"].as_str().unwrap_or_default().to_owned(),
            signature: json::string(&block["signature"]),
        })),
        "redacted_thinking" => Some(Body::Thinking(Thinking {
            text: String::new(),
            signature: json::string(&block["data"]),
        })),
        "tool_use" => Some(Body::ToolCall(ToolCall {
            call_id: normalize::call_id(&block["id"])?,
            name: block["name"].as_str().unwrap_or("unknown").to_owned(),
            input: normalize::tool_input(block["input"].clone()),
        })),
        _ => None,
    };
    Ok(body)
}

fn text_of(content: &Value) -> Option<String> {
    let text = match content {
        Value::Array(blocks) => json::texts(blocks, &["text"]),
        other => other.as_str().unwrap_or_default().to_owned(),
    };
    (!text.trim().is_empty()).then_some(text)
}
