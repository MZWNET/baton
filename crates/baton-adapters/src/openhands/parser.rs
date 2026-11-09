use anyhow::Result;
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

pub(super) fn drafts(event: &Value, fallback: &str, blobs: &Blobs) -> Result<Vec<Draft>> {
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
                call_id: normalize::call_id(&event["tool_call_id"]),
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
            )]
        }
        // Both stand in for a result the model saw
        "UserRejectObservation" => {
            let reason = event["rejection_reason"].as_str().unwrap_or_default();
            vec![result(
                event,
                format!("Action rejected: {reason}"),
                Vec::new(),
                true,
            )]
        }
        "AgentErrorEvent" => {
            let error = event["error"].as_str().unwrap_or_default().to_owned();
            vec![result(event, error, Vec::new(), true)]
        }
        "Condensation" => vec![match json::string(&event["summary"]) {
            Some(summary) => Body::Compaction(Compaction { summary }),
            None => Body::Meta(json!({"type": "condensation"})),
        }],
        _ => Vec::new(),
    };
    let timestamp = event["timestamp"]
        .as_str()
        .and_then(timestamp)
        .unwrap_or_else(|| fallback.to_owned());
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

// OpenHands stamps events with the naive local time
pub(super) fn timestamp(value: &str) -> Option<String> {
    if let Some(normalized) = time::normalize(value) {
        return Some(normalized);
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    time::from_unix_millis(local.timestamp_millis())
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

fn result(event: &Value, text: String, images: Vec<Image>, is_error: bool) -> Body {
    Body::ToolResult(ToolResult {
        call_id: normalize::call_id(&event["tool_call_id"]),
        text,
        images,
        is_error,
    })
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
