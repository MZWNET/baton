use anyhow::Result;
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use serde_json::Value;

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Devin's own summary preamble, which baton also writes in front of its summaries
pub(super) const SUMMARY_PREAMBLE: &str = "You are continuing work from a previous conversation thread. Below is a summary of the previous conversation thread:\n";
// Marks the summaries baton writes, in the extensions Devin keeps when it copies a node
pub(super) const COMPACTION_EXTENSION: &str = "baton/compaction";

// A node of the session's main chain
pub(super) struct Node {
    pub(super) id: i64,
    pub(super) chat_message: String,
    pub(super) created: i64,
    pub(super) summarized: bool,
}

pub(super) fn drafts(node: &Node, blobs: &Blobs) -> Result<Vec<Draft>> {
    let Ok(message @ Value::Object(_)) = serde_json::from_str::<Value>(&node.chat_message) else {
        return Ok(Vec::new());
    };
    let metadata = &message["metadata"];
    let content = content(&message, blobs)?;
    let role = message["role"].as_str().unwrap_or_default();
    let bodies = match (role, summary(node, &message)) {
        (_, Some(summary)) => vec![Body::Compaction(Compaction {
            summary: summary.to_owned(),
        })],
        // Other user messages are context the CLI injects, not typed input
        ("user", None) if metadata["is_user_input"] == true => {
            normalize::user(content).into_iter().collect()
        }
        ("assistant", None) => {
            let mut bodies = Vec::new();
            let thinking = Thinking {
                text: message["thinking"]["thinking"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                signature: json::string(&message["thinking"]["signature"]),
            };
            if !thinking.text.is_empty() || thinking.signature.is_some() {
                bodies.push(Body::Thinking(thinking));
            }
            bodies.extend(normalize::assistant(&content.text));
            let calls = message["tool_calls"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            for call in calls {
                bodies.push(Body::ToolCall(ToolCall {
                    call_id: normalize::call_id(&call["id"])?,
                    name: call["name"].as_str().unwrap_or("unknown").to_owned(),
                    input: normalize::arguments(&call["arguments"]),
                }));
            }
            bodies
        }
        ("tool", None) => {
            let outcome = &metadata["extensions"]["chisel/tool_result_meta"];
            vec![Body::ToolResult(ToolResult {
                call_id: normalize::call_id(&message["tool_call_id"])?,
                text: content.text,
                images: content.images,
                is_error: outcome["success"] == false,
            })]
        }
        _ => Vec::new(),
    };
    let timestamp = metadata["created_at"]
        .as_str()
        .and_then(time::normalize)
        .or_else(|| time::from_unix_millis(node.created * 1000))
        .unwrap_or_else(time::now);
    let native_entry_id =
        json::string(&message["message_id"]).unwrap_or_else(|| node.id.to_string());
    let mut drafts = Vec::with_capacity(bodies.len());
    for body in bodies {
        drafts.push(Draft {
            timestamp: timestamp.clone(),
            native_entry_id: Some(native_entry_id.clone()),
            body,
        });
    }
    Ok(drafts)
}

// The summary of a node baton wrote, or of one Devin wrote when it compacted
fn summary<'a>(node: &Node, message: &'a Value) -> Option<&'a str> {
    let marked = message["metadata"]["extensions"][COMPACTION_EXTENSION] == true;
    if !marked && !node.summarized {
        return None;
    }
    message["content"].as_str()?.strip_prefix(SUMMARY_PREAMBLE)
}

fn content(message: &Value, blobs: &Blobs) -> Result<Content> {
    let text = match &message["content"] {
        Value::Array(parts) => json::texts(parts, &["text"]),
        other => other.as_str().unwrap_or_default().to_owned(),
    };
    let entries = message["images"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut attached = Vec::new();
    for entry in entries {
        attached.extend(image(entry, blobs)?);
    }
    Ok(Content {
        text,
        images: attached,
    })
}

fn image(entry: &Value, blobs: &Blobs) -> Result<Option<Image>> {
    let mime = entry["mime_type"].as_str().or(entry["media_type"].as_str());
    let bytes = entry["base64_data"].as_str().and_then(images::decode);
    match (mime, bytes) {
        (Some(mime), Some(bytes)) => images::store(blobs, mime, &bytes).map(Some),
        _ => Ok(None),
    }
}
