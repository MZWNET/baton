use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Body, Event, Image};
use serde_json::{Value, json};

use super::parser;
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images, local_time};

// The app's private upload folder on the phone, where restoring a backup puts its `upload/` entries
const UPLOADS: &str = "file:///data/user/0/me.rerere.rikkahub/files/upload/";

// An attachment the backup holds as `upload/<name>`
pub(super) struct Upload {
    pub(super) name: String,
    pub(super) blob: String,
}

// Results go into the parts of their calls, where the app sends them from, so the order of the events is recorded beside them
pub(super) fn messages(events: &[&Event]) -> Result<(Vec<Value>, Vec<Upload>)> {
    let mut messages: Vec<Value> = Vec::new();
    let mut uploads = Vec::new();
    // The message and part of each call still waiting for its result
    let mut unanswered: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut open: Option<usize> = None;
    let mut order = 0;
    for unit in unit::units(events) {
        match unit {
            Unit::User(event, user) => {
                let mut parts = Vec::new();
                if !user.text.is_empty() || user.images.is_empty() {
                    parts.push(text(&user.text));
                }
                for image in &user.images {
                    parts.push(attach(image, &mut uploads));
                }
                record(&mut parts[0], event, &mut order);
                messages.push(message(event, "user", parts, false)?);
                open = None;
            }
            Unit::Compaction(event, summary) => {
                let mut summary = text(summary);
                record(&mut summary, event, &mut order);
                messages.push(message(event, "user", vec![summary], true)?);
                open = None;
            }
            Unit::Assistant(run) => {
                // The app sends consecutive tools that ran as one step, so a call after a result of the step opens another message
                let step_ran = open.is_some_and(|index| {
                    messages[index]["parts"]
                        .as_array()
                        .expect("messages hold a list of parts")
                        .iter()
                        .rev()
                        .take_while(|part| part["type"] == "tool")
                        .any(parser::ran)
                });
                if open.is_none() || (step_ran && matches!(run[0].body, Body::ToolCall(_))) {
                    messages.push(message(run[0], "assistant", Vec::new(), false)?);
                    open = Some(messages.len() - 1);
                }
                let index = open.expect("an assistant message is open");
                let parts = messages[index]["parts"]
                    .as_array_mut()
                    .expect("messages hold a list of parts");
                for event in run {
                    let mut part = match &event.body {
                        Body::Assistant(assistant) => text(&assistant.text),
                        Body::ToolCall(call) => {
                            unanswered.insert(&call.call_id, (index, parts.len()));
                            json!({
                                "type": "tool",
                                "toolCallId": call.call_id,
                                "toolName": call.name,
                                "input": call.input.to_string(),
                                "output": [],
                            })
                        }
                        _ => unreachable!("assistant runs hold only texts and calls"),
                    };
                    record(&mut part, event, &mut order);
                    parts.push(part);
                }
            }
            Unit::ToolResult(event, result) => {
                let call = unanswered.remove(result.call_id.as_str());
                // An empty text still marks the tool as run; the app sends a result only after its call, so one whose call is not written here reaches the model as a notice, the app's way of telling it about events
                let mut first = match call {
                    Some(_) => text(&result.text),
                    None => json!({
                        "type": "notice",
                        "title": "Tool result",
                        "text": result.text,
                        "metadata": {"baton": {"call": result.call_id}},
                    }),
                };
                record(&mut first, event, &mut order);
                if result.is_error {
                    first["metadata"]["baton"]["error"] = json!(true);
                }
                let mut parts = vec![first];
                for image in &result.images {
                    parts.push(attach(image, &mut uploads));
                }
                match call {
                    Some((message_index, part_index)) => {
                        messages[message_index]["parts"][part_index]["output"] =
                            Value::Array(parts);
                    }
                    None => {
                        messages.push(message(event, "user", parts, false)?);
                        open = None;
                    }
                }
            }
        }
    }

    Ok((messages, uploads))
}

fn message(event: &Event, role: &str, parts: Vec<Value>, checkpoint: bool) -> Result<Value> {
    let created = local_time::naive(&event.timestamp)?.format("%Y-%m-%dT%H:%M:%S%.3f");

    Ok(json!({
        "id": identifiers::uuid_v4(),
        "role": role,
        "parts": parts,
        "createdAt": created.to_string(),
        "isContextCheckpoint": checkpoint,
    }))
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

// The place and exact time of the event a part carries, which the app's own fields cannot hold, under a metadata key the app keeps without reading
fn record(part: &mut Value, event: &Event, order: &mut u64) {
    part["metadata"]["baton"]["order"] = json!(*order);
    part["metadata"]["baton"]["time"] = json!(event.timestamp);
    *order += 1;
}

// The image part, and the attachment it points to, named after its blob; the name cannot carry every media type, so the part records it
fn attach(image: &Image, uploads: &mut Vec<Upload>) -> Value {
    let name = format!("{}.{}", image.blob, images::extension(&image.mime));
    let url = format!("{UPLOADS}{name}");
    uploads.push(Upload {
        name,
        blob: image.blob.clone(),
    });
    json!({"type": "image", "url": url, "metadata": {"baton": {"mime": image.mime}}})
}
