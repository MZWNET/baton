use std::collections::BTreeMap;

use anyhow::Result;
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json, local_time};

pub(super) fn drafts(
    event: &Value,
    recorded: &BTreeMap<String, String>,
    fallback: &str,
    blobs: &Blobs,
) -> Result<Vec<Draft>> {
    let bodies = match event["kind"].as_str().unwrap_or_default() {
        "MessageEvent" => {
            let message = &event["llm_message"];
            let blocks = items(&message["content"]);
            if event["source"] == "user" {
                normalize::user(content(blocks, blobs)?)
                    .into_iter()
                    .collect()
            } else {
                let mut bodies =
                    thinking(&message["thinking_blocks"], &message["reasoning_content"]);
                for block in blocks {
                    bodies.extend(block["text"].as_str().and_then(normalize::assistant));
                }
                bodies
            }
        }
        // The thought of an action is the text the model wrote before its call
        "ActionEvent" => {
            let mut bodies = thinking(&event["thinking_blocks"], &event["reasoning_content"]);
            for block in items(&event["thought"]) {
                bodies.extend(block["text"].as_str().and_then(normalize::assistant));
            }
            bodies.push(Body::ToolCall(ToolCall {
                call_id: normalize::call_id(&event["tool_call_id"])?,
                name: event["tool_name"].as_str().unwrap_or("unknown").to_owned(),
                input: normalize::arguments(&event["tool_call"]["arguments"]),
            }));
            bodies
        }
        "ObservationEvent" => {
            let observation = &event["observation"];
            let content = content(items(&observation["content"]), blobs)?;
            vec![result(
                event,
                content.text,
                content.images,
                observation["is_error"] == true,
            )?]
        }
        // Both stand in for a result the model saw
        "UserRejectObservation" => {
            let reason = event["rejection_reason"].as_str().unwrap_or_default();
            vec![result(
                event,
                format!("Action rejected: {reason}"),
                Vec::new(),
                true,
            )?]
        }
        "AgentErrorEvent" => {
            let error = event["error"].as_str().unwrap_or_default().to_owned();
            vec![result(event, error, Vec::new(), true)?]
        }
        "Condensation" => vec![match json::string(&event["summary"]) {
            Some(summary) => Body::Compaction(Compaction { summary }),
            None => Body::Meta(json!({"type": "condensation"})),
        }],
        _ => Vec::new(),
    };
    let timestamp = time(event, recorded)?.unwrap_or_else(|| fallback.to_owned());
    let mut drafts = Vec::with_capacity(bodies.len());
    for body in bodies {
        drafts.push(Draft {
            timestamp: timestamp.clone(),
            native_entry_id: json::string(&event["id"]),
            body,
        });
    }
    Ok(drafts)
}

// The exact time baton recorded when it wrote the event, or else the naive local time OpenHands stamps events with
pub(super) fn time(event: &Value, recorded: &BTreeMap<String, String>) -> Result<Option<String>> {
    if let Some(time) = json::string(&event["id"]).and_then(|id| recorded.get(&id)) {
        return Ok(Some(time.clone()));
    }
    let Some(value) = event["timestamp"].as_str() else {
        return Ok(None);
    };

    Ok(Some(timestamp(value)?))
}

pub(super) fn timestamp(value: &str) -> Result<String> {
    match time::normalize(value) {
        Some(normalized) => Ok(normalized),
        None => local_time::parse(value),
    }
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn content(blocks: &[Value], blobs: &Blobs) -> Result<Content> {
    let mut attached = Vec::new();
    for block in blocks {
        for url in items(&block["image_urls"]) {
            attached.extend(images::from_data_url(
                blobs,
                url.as_str().unwrap_or_default(),
            )?);
        }
    }
    Ok(Content {
        text: json::texts(blocks, &["text"]),
        images: attached,
    })
}

fn result(event: &Value, text: String, images: Vec<Image>, is_error: bool) -> Result<Body> {
    Ok(Body::ToolResult(ToolResult {
        call_id: normalize::call_id(&event["tool_call_id"])?,
        text,
        images,
        is_error,
    }))
}

fn thinking(blocks: &Value, reasoning: &Value) -> Vec<Body> {
    let mut bodies = Vec::new();
    for block in items(blocks) {
        let thinking = match block["type"].as_str() {
            Some("redacted_thinking") => Thinking {
                text: String::new(),
                signature: json::string(&block["data"]),
            },
            _ => Thinking {
                text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                signature: json::string(&block["signature"]),
            },
        };
        if !thinking.text.is_empty() || thinking.signature.is_some() {
            bodies.push(Body::Thinking(thinking));
        }
    }
    if let Some(text) = json::string(reasoning) {
        bodies.push(Body::Thinking(Thinking {
            text,
            signature: None,
        }));
    }
    bodies
}
