use std::path::{Path, PathBuf};

use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolResult};
use serde_json::Value;

use crate::support::normalize::{self, Content};
use crate::support::{images, json, unit};

// Lines baton adds only to keep Vibe's tool calls answered, which the reader leaves out
pub(super) const SYNTHETIC_PREFIX: &str = "baton-synthetic-";
const ERROR_OPEN: &str = "<tool_error>";
const ERROR_CLOSE: &str = "</tool_error>";
const CANCELLED_OPEN: &str = "<user_cancellation>";
const CANCELLED_CLOSE: &str = "</user_cancellation>";
// The envelope older Vibe versions wrapped a compaction summary in
const SUMMARY_TAGS: [&str; 4] = [
    "<previous_user_messages>",
    "</previous_user_messages>",
    "<compaction_summary>",
    "</compaction_summary>",
];

// Reads the messages of a Vibe session, which carry neither ids nor timestamps
pub(super) struct MessageParser<'a> {
    blobs: &'a Blobs,
    // File images are relative to the session folder when Vibe did not record them absolute
    dir: PathBuf,
    last: Option<String>,
    timestamp: String,
}

impl<'a> MessageParser<'a> {
    pub(super) fn new(
        blobs: &'a Blobs,
        dir: &Path,
        leaf: Option<&str>,
        timestamp: String,
    ) -> MessageParser<'a> {
        MessageParser {
            blobs,
            dir: dir.to_path_buf(),
            last: leaf.map(String::from),
            timestamp,
        }
    }

    fn bodies(&self, message: &Value) -> Result<Vec<Body>> {
        let content = match &message["content"] {
            Value::Array(parts) => json::texts(parts, &["text"]),
            other => other.as_str().unwrap_or_default().to_owned(),
        };
        let injected = message["injected"] == true;
        let bodies = match message["role"].as_str().unwrap_or_default() {
            // Every request to the model starts at the latest compaction, which keeps the user messages before it
            "user"
                if message["context_boundary"] == "compaction"
                    || (injected && SUMMARY_TAGS.iter().all(|tag| content.contains(tag))) =>
            {
                vec![Body::Compaction(Compaction { summary: content })]
            }
            "user" if !injected => {
                let attached = message["images"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut found = Vec::new();
                for attachment in attached {
                    found.extend(self.image(attachment)?);
                }
                normalize::user(Content {
                    text: content,
                    images: found,
                })
                .into_iter()
                .collect()
            }
            "assistant"
                if !message["message_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with(SYNTHETIC_PREFIX)) =>
            {
                let mut bodies = Vec::new();
                if let Some(text) = json::string(&message["reasoning_content"]) {
                    let payloads = &message["reasoning_payloads"];
                    let signature = (!payloads.is_null()).then(|| payloads.to_string());
                    bodies.push(Body::Thinking(Thinking { text, signature }));
                }
                bodies.extend(normalize::assistant(&content));
                let calls = message["tool_calls"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for call in calls {
                    bodies.push(Body::ToolCall(unit::decode_function_call(call, &call["id"])));
                }
                bodies
            }
            "tool" => {
                let result = &message["tool_result"];
                let marker = &result["output"]["baton"];
                if marker["synthetic"] == true {
                    return Ok(Vec::new());
                }
                vec![Body::ToolResult(if marker.is_object() {
                    self.baton_result(message, content, marker)?
                } else {
                    native_result(message, content, result["cancelled"] == true)
                })]
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    // baton keeps the error flag and the images, which Vibe has no place for, in the result's private output
    fn baton_result(&self, message: &Value, content: String, marker: &Value) -> Result<ToolResult> {
        let is_error = marker["is_error"] == true;
        let text = match unwrapped(&content, ERROR_OPEN, ERROR_CLOSE) {
            Some(text) if is_error => text.to_owned(),
            _ => content,
        };
        let entries = marker["images"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut attached = Vec::new();
        for entry in entries {
            if let (Some(mime), Some(bytes)) = (
                entry["mime_type"].as_str(),
                entry["data"].as_str().and_then(images::decode),
            ) {
                attached.push(images::store(self.blobs, mime, &bytes)?);
            }
        }
        Ok(ToolResult {
            call_id: normalize::call_id(&message["tool_call_id"]),
            text,
            images: attached,
            is_error,
        })
    }

    fn image(&self, attachment: &Value) -> Result<Option<Image>> {
        let source = if attachment["source"].is_object() {
            &attachment["source"]
        } else {
            attachment
        };
        let Some(mime) = attachment["mime_type"].as_str() else {
            return Ok(None);
        };
        if let Some(bytes) = source["data"].as_str().and_then(images::decode) {
            return images::store(self.blobs, mime, &bytes).map(Some);
        }
        let Some(path) = source["path"].as_str() else {
            return Ok(None);
        };
        images::from_file(self.blobs, &self.dir.join(path))
    }
}

// Vibe wraps a whole failed or cancelled result in a tag that tells the model what happened
fn native_result(message: &Value, content: String, cancelled: bool) -> ToolResult {
    let (text, wrapped) = match unwrapped(&content, ERROR_OPEN, ERROR_CLOSE)
        .or_else(|| unwrapped(&content, CANCELLED_OPEN, CANCELLED_CLOSE))
    {
        Some(text) => (text.to_owned(), true),
        None => (content, false),
    };
    ToolResult {
        call_id: normalize::call_id(&message["tool_call_id"]),
        text,
        images: Vec::new(),
        is_error: wrapped || cancelled,
    }
}

fn unwrapped<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    text.strip_prefix(open)?.strip_suffix(close)
}

impl Parser for MessageParser<'_> {
    fn line(&mut self, offset: u64, message: Value) -> Result<Line> {
        let bodies = self.bodies(&message)?;
        if bodies.is_empty() {
            return Ok(Line::Skip);
        }
        // Lines have no ids, so their byte offsets stand in
        let id = offset.to_string();
        let parent = self.last.replace(id.clone());
        let mut drafts = Vec::with_capacity(bodies.len());
        for body in bodies {
            drafts.push(Draft {
                timestamp: self.timestamp.clone(),
                native_entry_id: json::string(&message["message_id"]),
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

    // The session facts live in `meta.json`, which the adapter reads
    fn finish(self) -> Info {
        Info::default()
    }
}
