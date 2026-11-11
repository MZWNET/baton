use anyhow::Result;
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, NativeRead, Progress, Thinking, ToolCall,
    ToolResult, time,
};
use serde_json::{Value, json};

use crate::opencode::tool_order;
use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// An OpenCode 2 export: flat typed messages, with each tool's result in its call's state
pub(in crate::opencode) fn parse(data: &Value, blobs: &Blobs) -> Result<NativeRead> {
    let session = &data["info"];
    let created = session["time"]["created"]
        .as_i64()
        .and_then(time::from_unix_millis);
    let fallback = created.clone().unwrap_or_else(time::now);
    let mut drafts = Vec::new();
    let messages = data["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for message in messages {
        let timestamp = json::timestamp_from_millis(&message["time"]["created"], &fallback);
        let mut bodies = Vec::new();
        match message["type"].as_str().unwrap_or_default() {
            "user" => {
                let files = message["files"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut attached = Vec::new();
                for file in files {
                    attached.extend(file_image(blobs, file)?);
                }
                let content = Content {
                    text: message["text"].as_str().unwrap_or_default().to_owned(),
                    images: attached,
                };
                bodies.extend(normalize::user(content));
            }
            // Results follow the message whose calls produced them, where the model saw them
            "assistant" => {
                let content = message["content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut results = Vec::new();
                for block in content {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            bodies.extend(block["text"].as_str().and_then(normalize::assistant))
                        }
                        "reasoning" => {
                            if let Some(text) = json::string(&block["text"]) {
                                bodies.push(Body::Thinking(Thinking {
                                    text,
                                    signature: None,
                                }));
                            }
                        }
                        "tool" => {
                            if tool_order::synthetic_call_id(&block["state"]).is_none() {
                                bodies.push(Body::ToolCall(ToolCall {
                                    call_id: normalize::call_id(&block["id"])?,
                                    name: block["name"].as_str().unwrap_or("unknown").to_owned(),
                                    input: normalize::tool_input(block["state"]["input"].clone()),
                                }));
                            }
                            results.extend(tool_result(blobs, block)?);
                        }
                        _ => {}
                    }
                }
                bodies.extend(results);
            }
            "compaction" if message["status"] == "completed" => {
                // A provider compaction keeps only an opaque checkpoint, so the visible history stays in force
                bodies.push(match json::string(&message["summary"]) {
                    Some(summary) => Body::Compaction(Compaction { summary }),
                    None => Body::Meta(json!({"type": "compaction"})),
                });
            }
            "model-switched" => {
                let model = &message["model"];
                let name = format!(
                    "{}/{}",
                    model["providerID"].as_str().unwrap_or_default(),
                    model["id"].as_str().unwrap_or_default()
                );
                bodies.push(Body::Meta(json!({"type": "model", "model": name})));
            }
            _ => {}
        }
        for body in bodies {
            drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: json::string(&message["id"]),
                body,
            });
        }
    }
    let info = Info {
        cwd: json::string(&session["location"]["directory"]),
        title: json::string(&session["title"]),
        created,
    };
    Ok(NativeRead {
        info,
        drafts,
        continued: false,
        progress: Progress::default(),
    })
}

// Running and streaming tools have no result yet
fn tool_result(blobs: &Blobs, tool: &Value) -> Result<Option<Body>> {
    let state = &tool["state"];
    let blocks = state["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut attached = Vec::new();
    for block in blocks {
        if block["type"] == "file" {
            attached.extend(images::from_data_url(
                blobs,
                block["uri"].as_str().unwrap_or_default(),
            )?);
        }
    }
    let (text, is_error) = match state["status"].as_str().unwrap_or_default() {
        "completed" => (json::texts(blocks, &["text"]), false),
        "error" => (
            state["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            true,
        ),
        _ => return Ok(None),
    };
    Ok(Some(Body::ToolResult(ToolResult {
        call_id: tool_order::synthetic_call_id(state)
            .map_or_else(|| normalize::call_id(&tool["id"]), |id| Ok(id.to_owned()))?,
        text,
        images: attached,
        is_error,
    })))
}

// Inline files carry their bytes as base64, others refer to a URI that only a data URL makes portable
fn file_image(blobs: &Blobs, file: &Value) -> Result<Option<Image>> {
    let Some(mime) = file["mime"]
        .as_str()
        .filter(|mime| mime.starts_with("image/"))
    else {
        return Ok(None);
    };
    if file["source"]["type"] == "uri" {
        return images::from_data_url(blobs, file["source"]["uri"].as_str().unwrap_or_default());
    }
    match file["data"].as_str().and_then(images::decode) {
        Some(bytes) => images::store(blobs, mime, &bytes).map(Some),
        None => Ok(None),
    }
}
