use anyhow::Result;
use baton_core::{Blobs, Body, Event, Export, Image, ToolResult, time};
use serde_json::{Value, json};

use crate::opencode::ascending_ids::AscendingIds;
use crate::opencode::tool_order::{self, Placement, ToolOrder};
use crate::support::images;
use crate::support::unit::{self, Unit};

// OpenCode 2 drops assistant messages without a completion time on import
pub(in crate::opencode) fn write(
    export: &Export,
    session_id: &str,
    blobs: &Blobs,
) -> Result<Value> {
    let mut ids = AscendingIds::default();
    let mut messages: Vec<Value> = Vec::new();
    let mut calls = ToolOrder::default();
    for unit in unit::units(export.events) {
        match unit {
            Unit::User(event, user) => {
                calls.clear();
                let millis = time::unix_millis(&event.timestamp);
                let mut message = json!({"id": ids.next("msg", millis), "type": "user", "time": {"created": millis}, "text": user.text});
                if !user.images.is_empty() {
                    let mut files = Vec::with_capacity(user.images.len());
                    for image in &user.images {
                        let data = images::encode(&blobs.get(&image.blob)?);
                        let name = format!("image.{}", images::extension(&image.mime));
                        files.push(json!({"data": data, "mime": image.mime, "source": {"type": "inline"}, "name": name}));
                    }
                    message["files"] = json!(files);
                }
                messages.push(message);
            }
            Unit::Assistant(run) => {
                let millis = time::unix_millis(&run[0].timestamp);
                let mut content = Vec::new();
                for &event in &run {
                    match &event.body {
                        Body::Assistant(assistant) => {
                            content.push(json!({"type": "text", "text": assistant.text}));
                        }
                        Body::ToolCall(call) => {
                            calls.insert(call, messages.len(), content.len());
                            content.push(json!({"type": "tool", "id": call.call_id, "name": call.name,
                                                "state": {"status": "running", "input": call.input, "metadata": {}},
                                                "time": {"created": millis}}));
                        }
                        _ => {}
                    }
                }
                messages.push(assistant(&mut ids, millis, content));
                calls.assistant(&run);
            }
            Unit::ToolResult(event, result) => match calls.result(result) {
                Placement::InPlace { message, part } => {
                    settle(
                        &mut messages[message]["content"][part],
                        event,
                        result,
                        blobs,
                        None,
                    )?;
                }
                Placement::Synthetic(call) => {
                    let synthetic = tool_order::synthetic(result, call);
                    let millis = time::unix_millis(&event.timestamp);
                    let mut tool = json!({"type": "tool", "id": synthetic.id, "name": synthetic.name,
                            "state": {"status": "running", "input": synthetic.input, "metadata": {}}, "time": {"created": millis}});
                    settle(&mut tool, event, result, blobs, Some(synthetic.marker))?;
                    messages.push(assistant(&mut ids, millis, vec![tool]));
                }
            },
            Unit::Compaction(event, summary) => {
                calls.clear();
                let millis = time::unix_millis(&event.timestamp);
                messages.push(json!({"id": ids.next("msg", millis), "type": "compaction", "time": {"created": millis},
                                     "status": "completed", "reason": "manual", "summary": summary}));
            }
        }
    }
    let created = time::unix_millis(export.created);
    let mut updated = created;
    for message in &messages {
        if let Some(millis) = message["time"]["created"].as_i64() {
            updated = updated.max(millis);
        }
    }
    let mut info = json!({
        "id": session_id,
        "projectID": "global",
        "location": {"directory": export.cwd.to_string_lossy()},
        "cost": 0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "time": {"created": created, "updated": updated},
    });
    if let Some(title) = export.title {
        info["title"] = json!(title);
    }
    Ok(json!({"info": info, "messages": messages}))
}

fn assistant(ids: &mut AscendingIds, millis: i64, content: Vec<Value>) -> Value {
    let finish = if content.iter().any(|block| block["type"] == "tool") {
        "tool-calls"
    } else {
        "stop"
    };
    json!({
        "id": ids.next("msg", millis), "type": "assistant", "time": {"created": millis, "completed": millis},
        "agent": "build", "model": {"id": "baton", "providerID": "baton"}, "content": content,
        "finish": finish, "cost": 0, "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
    })
}

// A completed tool needs at least one content block, so its text block is written even when empty
fn settle(
    tool: &mut Value,
    event: &Event,
    result: &ToolResult,
    blobs: &Blobs,
    marker: Option<Value>,
) -> Result<()> {
    let files = files(&result.images, blobs)?;
    let input = tool["state"]["input"].take();
    tool["state"] = if result.is_error {
        let mut state = json!({"status": "error", "input": input, "error": {"type": "ToolError", "message": result.text}});
        if !files.is_empty() {
            state["content"] = json!(files);
        }
        state
    } else {
        let mut content = vec![json!({"type": "text", "text": result.text})];
        content.extend(files);
        json!({"status": "completed", "input": input, "content": content})
    };
    tool_order::set_marker(&mut tool["state"], marker);
    let created = tool["time"]["created"]
        .as_i64()
        .expect("tool has a creation time");
    tool["time"]["completed"] = json!(time::unix_millis(&event.timestamp).max(created));
    Ok(())
}

fn files(attached: &[Image], blobs: &Blobs) -> Result<Vec<Value>> {
    let mut files = Vec::with_capacity(attached.len());
    for image in attached {
        let uri = images::data_url(blobs, image)?;
        files.push(json!({"type": "file", "uri": uri, "mime": image.mime}));
    }
    Ok(files)
}
