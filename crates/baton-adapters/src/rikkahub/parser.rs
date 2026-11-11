use std::path::Path;

use anyhow::{Context, Result};
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use super::backup::{self, Unpacked};
use crate::support::normalize::{self, Content};
use crate::support::{images, json, local_time};

// A body with what baton recorded of it when it wrote the part it comes from
struct Parsed {
    order: Option<u64>,
    time: Option<String>,
    body: Body,
}

impl Parsed {
    fn of(part: &Value, body: Body) -> Parsed {
        let record = &part["metadata"]["baton"];
        Parsed {
            order: record["order"].as_u64(),
            time: record["time"].as_str().and_then(time::normalize),
            body,
        }
    }
}

// The selected messages of a conversation, in the order baton wrote their events where it did; parts the app added since stay after what came before them
pub(super) fn drafts(messages: &[Value], unpacked: &Unpacked, blobs: &Blobs) -> Result<Vec<Draft>> {
    let mut drafts = Vec::new();
    let mut last = None;
    for message in messages {
        let native_entry_id = json::string(&message["id"]);
        let parsed = parse(message, unpacked, blobs)?;
        // The app stamps a message with the local time of the phone, which carries no offset
        let created = parsed
            .iter()
            .any(|parsed| parsed.time.is_none())
            .then(|| created(message))
            .transpose()?;
        for parsed in parsed {
            last = last.max(parsed.order);
            let draft = Draft {
                timestamp: parsed
                    .time
                    .or_else(|| created.clone())
                    .expect("the message time stands in for missing ones"),
                native_entry_id: native_entry_id.clone(),
                body: parsed.body,
            };
            drafts.push(((parsed.order.or(last), drafts.len()), draft));
        }
    }
    drafts.sort_by_key(|(key, _)| *key);

    Ok(drafts.into_iter().map(|(_, draft)| draft).collect())
}

fn created(message: &Value) -> Result<String> {
    let created = message["createdAt"]
        .as_str()
        .with_context(|| format!("RikkaHub message {} has no createdAt", message["id"]))?;

    local_time::parse(created).with_context(|| {
        format!(
            "reading when RikkaHub message {} was created",
            message["id"]
        )
    })
}

fn parse(message: &Value, unpacked: &Unpacked, blobs: &Blobs) -> Result<Vec<Parsed>> {
    let parts = message["parts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut parsed = Vec::new();
    match message["role"].as_str() {
        // The app's context compression leaves a summary of everything before it as a user message
        Some("user") if message["isContextCheckpoint"] == true => {
            let summary = json::texts(parts, &["text"]);
            if !summary.trim().is_empty() {
                parsed.push(Parsed::of(
                    &parts[0],
                    Body::Compaction(Compaction { summary }),
                ));
            }
        }
        Some("user") => match parts.first() {
            // A result whose call baton did not write, which it wrote as a notice
            Some(notice)
                if notice["type"] == "notice"
                    && notice["metadata"]["baton"]["call"].is_string() =>
            {
                let record = &notice["metadata"]["baton"];
                let result = ToolResult {
                    call_id: normalize::call_id(&record["call"])?,
                    text: notice["text"].as_str().unwrap_or_default().to_owned(),
                    images: content(&parts[1..], unpacked, blobs)?.images,
                    is_error: record["error"] == true,
                };
                parsed.push(Parsed::of(notice, Body::ToolResult(result)));
            }
            Some(first) => {
                if let Some(user) = normalize::user(content(parts, unpacked, blobs)?) {
                    parsed.push(Parsed::of(first, user));
                }
            }
            None => {}
        },
        Some("assistant") => {
            // Consecutive tools that ran are one step of the model, its calls before their results, as the app sends them
            let mut step = Vec::new();
            for part in parts {
                if ran(part) {
                    step.push(part);
                    continue;
                }
                push_step(&mut step, &mut parsed, unpacked, blobs)?;
                match part["type"].as_str() {
                    Some("reasoning") => {
                        if let Some(text) = json::string(&part["reasoning"]) {
                            let thinking = Thinking {
                                text,
                                signature: None,
                            };
                            parsed.push(Parsed::of(part, Body::Thinking(thinking)));
                        }
                    }
                    Some("text") => {
                        if let Some(assistant) =
                            normalize::assistant(part["text"].as_str().unwrap_or_default())
                        {
                            parsed.push(Parsed::of(part, assistant));
                        }
                    }
                    // A tool that has not run, such as one still awaiting approval
                    Some("tool") => parsed.push(Parsed::of(part, Body::ToolCall(call(part)?))),
                    _ => {}
                }
            }
            push_step(&mut step, &mut parsed, unpacked, blobs)?;
        }
        _ => {}
    }

    Ok(parsed)
}

// Only its output tells that a tool ran
pub(super) fn ran(part: &Value) -> bool {
    part["type"] == "tool"
        && part["output"]
            .as_array()
            .is_some_and(|output| !output.is_empty())
}

fn call(part: &Value) -> Result<ToolCall> {
    let input = &part["input"];

    Ok(ToolCall {
        call_id: normalize::call_id(&part["toolCallId"])?,
        name: part["toolName"].as_str().unwrap_or("unknown").to_owned(),
        // The app reads blank arguments as none
        input: if input.as_str().is_some_and(|text| text.trim().is_empty()) {
            json!({})
        } else {
            normalize::arguments(input)
        },
    })
}

// The calls of a step, then their results
fn push_step(
    step: &mut Vec<&Value>,
    parsed: &mut Vec<Parsed>,
    unpacked: &Unpacked,
    blobs: &Blobs,
) -> Result<()> {
    for tool in step.iter() {
        parsed.push(Parsed::of(tool, Body::ToolCall(call(tool)?)));
    }
    for tool in step.drain(..) {
        let output = tool["output"]
            .as_array()
            .expect("a step only holds tools that ran");
        let content = content(output, unpacked, blobs)?;
        let result = ToolResult {
            call_id: normalize::call_id(&tool["toolCallId"])?,
            text: content.text,
            images: content.images,
            is_error: failed(output),
        };
        parsed.push(Parsed::of(&output[0], Body::ToolResult(result)));
    }

    Ok(())
}

// Baton marks the results it writes; the app reports a tool that failed or was denied as a JSON object holding only `error`
fn failed(output: &[Value]) -> bool {
    let record = &output[0]["metadata"]["baton"];
    if record.is_object() {
        return record["error"] == true;
    }
    let [part] = output else {
        return false;
    };
    let Some(Value::Object(object)) = part["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    else {
        return false;
    };

    part["type"] == "text" && object.len() == 1 && object.get("error").is_some_and(Value::is_string)
}

fn content(parts: &[Value], unpacked: &Unpacked, blobs: &Blobs) -> Result<Content> {
    let mut attached = Vec::new();
    for part in parts {
        if part["type"] == "image" {
            attached.extend(image(part, unpacked, blobs)?);
        }
    }

    Ok(Content {
        text: json::texts(parts, &["text"]),
        images: attached,
    })
}

// Attachments live in the app's upload folder, which the backup carries; remote images are left out
fn image(part: &Value, unpacked: &Unpacked, blobs: &Blobs) -> Result<Option<Image>> {
    let Some(url) = part["url"].as_str() else {
        return Ok(None);
    };
    if url.starts_with("data:") {
        return images::from_data_url(blobs, url);
    }
    let Some(name) = backup::upload_name(url) else {
        return Ok(None);
    };
    // Baton records the media type of the images it writes, which their names cannot always carry
    let Some(mime) = part["metadata"]["baton"]["mime"]
        .as_str()
        .or_else(|| images::mime(Path::new(name)))
    else {
        return Ok(None);
    };
    let Some(bytes) = unpacked.upload(name)? else {
        return Ok(None);
    };

    images::store(blobs, mime, &bytes).map(Some)
}
