use anyhow::Result;
use baton_core::{Blobs, Event, Image, time};
use serde_json::{Value, json};

use crate::devin::node_parser::{COMPACTION_EXTENSION, SUMMARY_PREAMBLE};
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images};

// A `message_nodes` row before it joins the chain
pub(super) struct Node {
    pub(super) chat_message: Value,
    pub(super) created: i64,
}

// One node per message, chained in order
pub(super) fn nodes(events: &[&Event], blobs: &Blobs) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    for unit in unit::units(events) {
        match unit {
            Unit::User(event, user) => {
                let mut message = message(event, "user", &user.text, Some(true));
                attach(&mut message, &user.images, blobs)?;
                nodes.push(node(event, message));
            }
            Unit::Assistant(run) => {
                for reply in unit::replies(&run) {
                    let mut calls = Vec::with_capacity(reply.calls.len());
                    for (index, call) in reply.calls.iter().enumerate() {
                        calls.push(
                            json!({"id": call.call_id, "name": call.name, "arguments": call.input,
                                          "index": index, "kind": "function"}),
                        );
                    }
                    let finish = if calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    };
                    let mut message = message(reply.event, "assistant", reply.text, None);
                    message["tool_calls"] = json!(calls);
                    message["metadata"]["finish_reason"] = json!(finish);
                    nodes.push(node(reply.event, message));
                }
            }
            Unit::ToolResult(event, result) => {
                let mut message = message(event, "tool", &result.text, None);
                message["tool_call_id"] = json!(result.call_id);
                message["metadata"]["extensions"] =
                    json!({"chisel/tool_result_meta": {"success": !result.is_error}});
                attach(&mut message, &result.images, blobs)?;
                nodes.push(node(event, message));
            }
            Unit::Compaction(event, summary) => {
                // The model sees it as a user message, but titles and search only pick up typed input
                let text = format!("{SUMMARY_PREAMBLE}{summary}");
                let mut message = message(event, "user", &text, Some(false));
                message["metadata"]["extensions"] = json!({COMPACTION_EXTENSION: true});
                nodes.push(node(event, message));
            }
        }
    }
    Ok(nodes)
}

// Every native message carries these metadata keys, so Devin's strict parsing finds them all
fn message(event: &Event, role: &str, content: &str, is_user_input: Option<bool>) -> Value {
    let telemetry = match role {
        "assistant" => json!({"source": "assistant", "operation": "inference"}),
        "tool" => json!({"source": "tool_result", "operation": "unknown"}),
        _ => json!({"source": "user", "operation": "unknown"}),
    };
    json!({
        "message_id": identifiers::uuid_v4(),
        "role": role,
        "content": content,
        "metadata": {
            "num_tokens": null,
            "is_user_input": is_user_input,
            "request_id": null,
            "metrics": null,
            "finish_reason": null,
            "created_at": event.timestamp,
            "telemetry": telemetry,
        },
    })
}

// baton keeps no image dimensions, which leaves them zero
fn attach(message: &mut Value, attached: &[Image], blobs: &Blobs) -> Result<()> {
    if attached.is_empty() {
        return Ok(());
    }
    let mut entries = Vec::with_capacity(attached.len());
    for image in attached {
        let data = images::encode(&blobs.get(&image.blob)?);
        entries
            .push(json!({"width": 0, "height": 0, "base64_data": data, "mime_type": image.mime}));
    }
    message["images"] = json!(entries);
    Ok(())
}

fn node(event: &Event, chat_message: Value) -> Node {
    Node {
        chat_message,
        created: time::unix_millis(&event.timestamp).div_euclid(1000),
    }
}
