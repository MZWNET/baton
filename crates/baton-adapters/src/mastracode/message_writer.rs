use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image, ToolResult, time};
use serde_json::{Value, json};

use crate::support::images;
use crate::support::unit::{self, Unit};

// A `mastra_messages` row before it gets its ids
pub(super) struct Message {
    pub(super) role: &'static str,
    pub(super) created: String,
    pub(super) content: Value,
}

// Results that directly follow their calls fold into them as MastraCode stores them; any other result and every summary become marked user rows, which MastraCode replays as user messages
pub(super) fn messages(events: &[&Event], blobs: &Blobs) -> Result<Vec<Message>> {
    let units = unit::units(events);
    let mut messages: Vec<Message> = Vec::new();
    let mut previous_millis = None;
    let mut index = 0;
    while index < units.len() {
        let (role, timestamp, content) = match &units[index] {
            Unit::User(event, user) => {
                let mut parts = Vec::new();
                if !user.text.is_empty() {
                    parts.push(json!({"type": "text", "text": user.text}));
                }
                parts.extend(files(&user.images, blobs)?);
                (
                    "user",
                    &event.timestamp,
                    json!({"format": 2, "parts": parts}),
                )
            }
            Unit::Assistant(run) => {
                let mut parts = Vec::new();
                for event in run {
                    match &event.body {
                        Body::Assistant(assistant) => {
                            parts.push(json!({"type": "text", "text": assistant.text}));
                        }
                        Body::ToolCall(call) => parts.push(json!({
                            "type": "tool-invocation",
                            "toolInvocation": {"state": "call", "toolCallId": call.call_id, "toolName": call.name, "args": call.input},
                        })),
                        _ => {}
                    }
                }
                let mut folded = Vec::new();
                while let Some(Unit::ToolResult(_, result)) = units.get(index + 1) {
                    let open = parts.iter_mut().find(|part| {
                        let invocation = &part["toolInvocation"];
                        invocation["state"] == "call"
                            && invocation["toolCallId"] == result.call_id.as_str()
                    });
                    let Some(part) = open else {
                        break;
                    };
                    fold(&mut part["toolInvocation"], result, blobs)?;
                    folded.push(result.call_id.as_str());
                    index += 1;
                }
                let mut content = json!({"format": 2, "parts": parts});
                if !in_call_order(&folded, &content["parts"]) {
                    content["metadata"] = json!({"baton": {"resultOrder": folded}});
                }
                ("assistant", &run[0].timestamp, content)
            }
            Unit::ToolResult(event, result) => {
                let marker = json!({"kind": "tool_result", "callId": result.call_id, "isError": result.is_error});
                let content = marked(&result.text, &result.images, marker, blobs)?;
                ("user", &event.timestamp, content)
            }
            Unit::Compaction(event, summary) => {
                let content = marked(summary, &[], json!({"kind": "compaction"}), blobs)?;
                ("user", &event.timestamp, content)
            }
        };
        // MastraCode orders rows by their creation time alone
        let mut millis = time::unix_millis(timestamp);
        if let Some(previous) = previous_millis {
            millis = millis.max(previous + 1);
        }
        let created = match time::from_unix_millis(millis) {
            Some(created) => created,
            None => {
                millis = time::unix_millis(timestamp);
                timestamp.clone()
            }
        };
        previous_millis = Some(millis);
        messages.push(Message {
            role,
            created,
            content,
        });
        index += 1;
    }
    Ok(messages)
}

fn fold(invocation: &mut Value, result: &ToolResult, blobs: &Blobs) -> Result<()> {
    if result.is_error && result.images.is_empty() {
        invocation["state"] = json!("output-error");
        invocation["errorText"] = json!(result.text);
        return Ok(());
    }
    invocation["state"] = json!("result");
    invocation["result"] = if result.images.is_empty() {
        json!(result.text)
    } else {
        // MastraCode hands MCP-shaped content with images to the model as a multimodal result
        let mut blocks = Vec::new();
        if !result.text.is_empty() {
            blocks.push(json!({"type": "text", "text": result.text}));
        }
        for image in &result.images {
            let data = images::encode(&blobs.get(&image.blob)?);
            blocks.push(json!({"type": "image", "data": data, "mimeType": image.mime}));
        }
        json!({"content": blocks})
    };
    if result.is_error {
        invocation["isError"] = json!(true);
    }
    Ok(())
}

// Whether results arrived in the order of their calls, which is how the reader lists them otherwise
fn in_call_order(folded: &[&str], parts: &Value) -> bool {
    let positions: Vec<Option<usize>> = folded
        .iter()
        .map(|id| {
            parts
                .as_array()
                .expect("parts are an array")
                .iter()
                .position(|part| part["toolInvocation"]["toolCallId"] == *id)
        })
        .collect();
    positions.is_sorted()
}

// A user row marked for the reader, with a text part even when empty because MastraCode needs one
fn marked(text: &str, attached: &[Image], marker: Value, blobs: &Blobs) -> Result<Value> {
    let mut parts = vec![json!({"type": "text", "text": text})];
    parts.extend(files(attached, blobs)?);
    Ok(json!({"format": 2, "parts": parts, "metadata": {"baton": marker}}))
}

fn files(attached: &[Image], blobs: &Blobs) -> Result<Vec<Value>> {
    let mut parts = Vec::with_capacity(attached.len());
    for image in attached {
        let data = images::encode(&blobs.get(&image.blob)?);
        parts.push(json!({"type": "file", "data": data, "mimeType": image.mime}));
    }
    Ok(parts)
}
