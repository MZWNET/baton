use anyhow::Result;
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use serde_json::Value;

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// One `mastra_messages` row
pub(super) struct Row {
    pub(super) id: String,
    pub(super) role: String,
    pub(super) created: String,
    pub(super) content: String,
}

pub(super) fn drafts(row: &Row, blobs: &Blobs) -> Result<Vec<Draft>> {
    // Observational memory hints and reminders of pauses are injected context
    if row.id == "om-continuation" || row.id.starts_with("__temporal_") {
        return Ok(Vec::new());
    }
    let Ok(content) = serde_json::from_str::<Value>(&row.content) else {
        return Ok(Vec::new());
    };
    if content["format"] != 2 {
        return Ok(Vec::new());
    }
    let parts = content["parts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let metadata = &content["metadata"];
    let marker = &metadata["baton"];
    let user_signal = matches!(
        metadata["signal"]["type"].as_str(),
        Some("user" | "user-message")
    );
    let bodies = match (row.role.as_str(), marker["kind"].as_str()) {
        (_, Some("compaction")) => {
            let summary = json::texts(parts, &["text"]);
            vec![Body::Compaction(Compaction { summary })]
        }
        (_, Some("tool_result")) => {
            let content = self::content(parts, blobs)?;
            vec![Body::ToolResult(ToolResult {
                call_id: normalize::call_id(&marker["callId"]),
                text: content.text,
                images: content.images,
                is_error: marker["isError"] == true,
            })]
        }
        ("user", _) => normalize::user(self::content(parts, blobs)?)
            .into_iter()
            .collect(),
        ("signal", _) if user_signal => normalize::user(self::content(parts, blobs)?)
            .into_iter()
            .collect(),
        ("assistant", _) => assistant(parts, &marker["resultOrder"], blobs)?,
        _ => Vec::new(),
    };
    let timestamp = time::normalize(&row.created).unwrap_or_else(time::now);
    let mut drafts = Vec::with_capacity(bodies.len());
    for body in bodies {
        drafts.push(Draft {
            timestamp: timestamp.clone(),
            native_entry_id: Some(row.id.clone()),
            body,
        });
    }
    Ok(drafts)
}

// Each step lists its text and calls, then the results MastraCode folded into the calls, which is how the model saw them
fn assistant(parts: &[Value], result_order: &Value, blobs: &Blobs) -> Result<Vec<Body>> {
    let order: Vec<&str> = result_order
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut bodies = Vec::new();
    for step in parts.split(|part| part["type"] == "step-start") {
        let mut results: Vec<(&str, Body)> = Vec::new();
        for part in step {
            match part["type"].as_str().unwrap_or_default() {
                "text" => bodies.extend(part["text"].as_str().and_then(normalize::assistant)),
                "reasoning" => bodies.extend(thinking(part)),
                "tool-invocation" => {
                    let invocation = &part["toolInvocation"];
                    bodies.push(call(invocation));
                    if let Some(result) = invocation_result(part, invocation, blobs)? {
                        results.push((
                            invocation["toolCallId"].as_str().unwrap_or_default(),
                            result,
                        ));
                    }
                }
                "tool-call" => bodies.push(call(part)),
                "tool-result" => {
                    let content = result_content(&part["result"], blobs)?;
                    let result = tool_result(part, content, part["isError"] == true);
                    results.push((part["toolCallId"].as_str().unwrap_or_default(), result));
                }
                _ => {}
            }
        }
        if !order.is_empty() {
            results.sort_by_key(|(id, _)| order.iter().position(|ordered| ordered == id));
        }
        bodies.extend(results.into_iter().map(|(_, result)| result));
    }
    Ok(bodies)
}

fn call(invocation: &Value) -> Body {
    Body::ToolCall(ToolCall {
        call_id: normalize::call_id(&invocation["toolCallId"]),
        name: invocation["toolName"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        input: normalize::tool_input(invocation["args"].clone()),
    })
}

fn tool_result(invocation: &Value, content: Content, is_error: bool) -> Body {
    Body::ToolResult(ToolResult {
        call_id: normalize::call_id(&invocation["toolCallId"]),
        text: content.text,
        images: content.images,
        is_error,
    })
}

// Pending calls have no result yet; a completed one is replayed from the output the model saw when MastraCode kept it
fn invocation_result(part: &Value, invocation: &Value, blobs: &Blobs) -> Result<Option<Body>> {
    let (content, is_error) = match invocation["state"].as_str().unwrap_or_default() {
        "result" => {
            let output = &part["providerMetadata"]["mastra"]["modelOutput"];
            let content = if output.is_null() {
                result_content(&invocation["result"], blobs)?
            } else {
                model_output(output, blobs)?
            };
            (content, invocation["isError"] == true)
        }
        "output-error" => (text(&invocation["errorText"]), true),
        "output-denied" => {
            let reason = invocation["approval"]["reason"]
                .as_str()
                .unwrap_or("Tool call was not approved by the user");
            (text(&Value::from(reason)), true)
        }
        _ => return Ok(None),
    };
    Ok(Some(tool_result(invocation, content, is_error)))
}

fn model_output(output: &Value, blobs: &Blobs) -> Result<Content> {
    let value = &output["value"];
    let content = match output["type"].as_str().unwrap_or_default() {
        "text" | "error-text" => text(value),
        "content" => {
            let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
            let mut attached = Vec::new();
            for item in items {
                if matches!(item["type"].as_str(), Some("media" | "image-data")) {
                    attached.extend(image(&item["data"], &item["mediaType"], blobs)?);
                }
            }
            Content {
                text: json::texts(items, &["text"]),
                images: attached,
            }
        }
        _ => text(&Value::from(value.to_string())),
    };
    Ok(content)
}

// A tool's own result: plain text, an MCP content list, or any other JSON
fn result_content(result: &Value, blobs: &Blobs) -> Result<Content> {
    if let Some(blocks) = result["content"].as_array() {
        let mut attached = Vec::new();
        for block in blocks {
            if block["type"] == "image" {
                attached.extend(image(&block["data"], &block["mimeType"], blobs)?);
            }
        }
        return Ok(Content {
            text: json::texts(blocks, &["text"]),
            images: attached,
        });
    }
    Ok(Content {
        text: json::to_text(result),
        images: Vec::new(),
    })
}

fn text(value: &Value) -> Content {
    Content {
        text: value.as_str().unwrap_or_default().to_owned(),
        images: Vec::new(),
    }
}

// The text and images of a user row, whose images MastraCode stores as file parts of raw base64 or data URLs
fn content(parts: &[Value], blobs: &Blobs) -> Result<Content> {
    let mut attached = Vec::new();
    for part in parts {
        let found = match part["type"].as_str().unwrap_or_default() {
            "file" if part["data"].is_string() => image(&part["data"], &part["mimeType"], blobs)?,
            "file" => image(&part["url"], &part["mediaType"], blobs)?,
            "image" => image(&part["image"], &part["mimeType"], blobs)?,
            _ => None,
        };
        attached.extend(found);
    }
    Ok(Content {
        text: json::texts(parts, &["text"]),
        images: attached,
    })
}

fn image(data: &Value, mime: &Value, blobs: &Blobs) -> Result<Option<Image>> {
    let (Some(data), Some(mime)) = (data.as_str(), mime.as_str()) else {
        return Ok(None);
    };
    if data.starts_with("data:") {
        return images::from_data_url(blobs, data);
    }
    match images::decode(data) {
        Some(bytes) if mime.starts_with("image/") => images::store(blobs, mime, &bytes).map(Some),
        _ => Ok(None),
    }
}

fn thinking(part: &Value) -> Option<Body> {
    let text = match part["reasoning"].as_str() {
        Some(text) => text.to_owned(),
        None => {
            let details = part["details"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            json::texts(details, &["text"])
        }
    };
    let metadata = &part["providerMetadata"];
    let signatures = [
        &metadata["anthropic"]["signature"],
        &metadata["anthropic"]["redactedData"],
        &metadata["openai"]["reasoningEncryptedContent"],
        &metadata["google"]["thoughtSignature"],
    ];
    let signature = signatures.into_iter().find_map(json::string);
    if text.is_empty() && signature.is_none() {
        return None;
    }
    Some(Body::Thinking(Thinking { text, signature }))
}
