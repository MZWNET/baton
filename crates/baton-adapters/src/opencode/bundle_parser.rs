use anyhow::Result;
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, NativeRead, Progress, Thinking, ToolCall,
    ToolResult, time,
};
use serde_json::Value;

use crate::opencode::bundle_writer::UNNAMED;
use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Tool results follow the step that produced them, where the model saw them, and a compaction request with its summary reply becomes one compaction
pub(super) fn parse_bundle(bundle: &Value, blobs: &Blobs) -> Result<NativeRead> {
    let session = &bundle["info"];
    let created = session["time"]["created"]
        .as_i64()
        .and_then(time::from_unix_millis);
    let fallback = created.clone().unwrap_or_else(time::now);
    let mut drafts = Vec::new();
    let mut compacting = false;
    let messages = bundle["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for message in messages {
        let head = &message["info"];
        let parts = message["parts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let timestamp = json::timestamp_from_millis(&head["time"]["created"], &fallback);
        let draft = |body: Body, native_entry_id: &Value| Draft {
            timestamp: timestamp.clone(),
            native_entry_id: json::string(native_entry_id),
            body,
        };
        match head["role"].as_str().unwrap_or_default() {
            "user" if parts.iter().any(|part| part["type"] == "compaction") => compacting = true,
            "user" => {
                let mut attached = Vec::new();
                for part in parts {
                    if part["type"] == "file" {
                        attached.extend(file_image(blobs, part)?);
                    }
                }
                let content = Content {
                    text: text(parts),
                    images: attached,
                };
                if let Some(body) = normalize::user(content) {
                    drafts.push(draft(body, &head["id"]));
                }
            }
            "assistant" if compacting && head["summary"] == true => {
                compacting = false;
                let summary = text(parts);
                if !summary.trim().is_empty() {
                    drafts.push(draft(Body::Compaction(Compaction { summary }), &head["id"]));
                }
            }
            "assistant" => {
                let mut results = Vec::new();
                for part in parts {
                    match part["type"].as_str().unwrap_or_default() {
                        "text" if part["ignored"] != true => {
                            if let Some(body) = part["text"].as_str().and_then(normalize::assistant)
                            {
                                drafts.push(draft(body, &part["id"]));
                            }
                        }
                        "reasoning" => {
                            if let Some(text) = json::string(&part["text"]) {
                                let thinking = Body::Thinking(Thinking {
                                    text,
                                    signature: None,
                                });
                                drafts.push(draft(thinking, &part["id"]));
                            }
                        }
                        "tool" => {
                            drafts.push(draft(tool_call(part), &part["id"]));
                            if let Some(result) = tool_result(blobs, part)? {
                                results.push(draft(result, &part["id"]));
                            }
                        }
                        "step-start" | "step-finish" => drafts.append(&mut results),
                        _ => {}
                    }
                }
                drafts.append(&mut results);
            }
            _ => {}
        }
    }
    let info = Info {
        cwd: json::string(&session["directory"]),
        title: named(json::string(&session["title"])),
        created,
    };
    Ok(NativeRead {
        info,
        drafts,
        continued: false,
        progress: Progress {
            offset: 0,
            tail_hash: String::new(),
            leaf: None,
        },
    })
}

// A placeholder title is no title, so it never replaces a real one
pub(super) fn named(title: Option<String>) -> Option<String> {
    title.filter(|title| !title.is_empty() && !title.starts_with(UNNAMED))
}

fn tool_call(part: &Value) -> Body {
    Body::ToolCall(ToolCall {
        call_id: normalize::call_id(&part["callID"]),
        name: part["tool"].as_str().unwrap_or("unknown").to_owned(),
        input: normalize::tool_input(part["state"]["input"].clone()),
    })
}

// Pending and running calls have no result yet
fn tool_result(blobs: &Blobs, part: &Value) -> Result<Option<Body>> {
    let state = &part["state"];
    let (text, attached, is_error) = match state["status"].as_str().unwrap_or_default() {
        "completed" => {
            let attachments = state["attachments"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut attached = Vec::new();
            for attachment in attachments {
                attached.extend(file_image(blobs, attachment)?);
            }
            (state["output"].as_str(), attached, false)
        }
        "error" => (state["error"].as_str(), Vec::new(), true),
        _ => return Ok(None),
    };
    Ok(Some(Body::ToolResult(ToolResult {
        call_id: normalize::call_id(&part["callID"]),
        text: text.unwrap_or_default().to_owned(),
        images: attached,
        is_error,
    })))
}

fn text(parts: &[Value]) -> String {
    let mut texts = Vec::new();
    for part in parts {
        if part["type"] == "text"
            && part["ignored"] != true
            && let Some(text) = part["text"].as_str()
        {
            texts.push(text);
        }
    }
    texts.join("\n")
}

fn file_image(blobs: &Blobs, part: &Value) -> Result<Option<Image>> {
    match (part["mime"].as_str(), part["url"].as_str()) {
        (Some(mime), Some(url)) if mime.starts_with("image/") => images::from_data_url(blobs, url),
        _ => Ok(None),
    }
}
