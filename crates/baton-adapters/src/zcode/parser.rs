use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json, tool_order};

pub(super) struct Message {
    pub(super) id: String,
    pub(super) data: Value,
    pub(super) parts: Vec<Value>,
}

pub(super) fn read(
    messages: Vec<Message>,
    revert: Option<String>,
    created: &str,
    blobs: &Blobs,
    artifacts: &Path,
) -> Result<Vec<Draft>> {
    let revert = revert
        .map(|value| serde_json::from_str::<Value>(&value).context("parsing ZCode session revert"))
        .transpose()?;
    let branch = active_branch(&messages, revert.as_ref());
    let mut prefixes: Vec<Vec<Draft>> = (0..branch.len()).map(|_| Vec::new()).collect();
    let mut entries: Vec<Vec<Draft>> = (0..branch.len()).map(|_| Vec::new()).collect();
    let mut model = None;

    for (index, message) in branch.iter().enumerate() {
        let timestamp = json::timestamp_from_millis(&message.data["time"]["created"], created);
        let draft = |body, id: Option<String>| Draft {
            timestamp: timestamp.clone(),
            native_entry_id: id,
            body,
        };
        let boundary = message.parts.iter().find(|part| {
            part["type"] == "compaction"
                && (!part["compactBoundary"].is_null() || part["timelineStatus"].is_null())
        });
        if let Some(boundary) = boundary {
            let summary = text(&message.parts, true);
            if !summary.trim().is_empty() {
                let entry = draft(
                    Body::Compaction(Compaction { summary }),
                    Some(message.id.clone()),
                );
                let head =
                    boundary["compactBoundary"]["preservedSegment"]["headMessageId"].as_str();
                if let Some(target) =
                    head.and_then(|head| branch[..index].iter().position(|m| m.id == head))
                {
                    prefixes[target].push(entry);
                } else {
                    entries[index].push(entry);
                }
            }
            continue;
        }
        match message.data["role"].as_str() {
            Some("user") if message.data["synthetic"] != true => {
                let mut attached = Vec::new();
                for part in &message.parts {
                    if part["type"] == "file" {
                        attached.extend(file_image(blobs, artifacts, part)?);
                    }
                }
                if let Some(body) = normalize::user(Content {
                    text: text(&message.parts, false),
                    images: attached,
                }) {
                    entries[index].push(draft(body, Some(message.id.clone())));
                }
            }
            Some("assistant") => {
                let mut last_tools = HashMap::new();
                for (part_index, part) in message.parts.iter().enumerate() {
                    if part["type"] == "tool"
                        && part["state"]["input"]["source"] != "browser_turn_end"
                    {
                        last_tools.insert(part["callID"].as_str(), part_index);
                    }
                }
                let complete_indices = message
                    .parts
                    .iter()
                    .filter(|part| {
                        matches!(part["type"].as_str(), Some("text" | "reasoning" | "tool"))
                            && !(part["type"] == "tool"
                                && part["state"]["input"]["source"] == "browser_turn_end")
                    })
                    .all(|part| part["blockIndex"].as_u64().is_some());
                let mut blocks: Vec<&Value> = message
                    .parts
                    .iter()
                    .enumerate()
                    .filter_map(|(part_index, part)| match part["type"].as_str() {
                        Some("text" | "reasoning") => Some(part),
                        Some("tool")
                            if last_tools.get(&part["callID"].as_str()) == Some(&part_index) =>
                        {
                            Some(part)
                        }
                        _ => None,
                    })
                    .collect();
                if complete_indices {
                    blocks.sort_by_key(|part| part["blockIndex"].as_u64());
                }
                let discarded = message.data["error"]["name"] == "StreamRecoveryDiscarded";
                let mut results = Vec::new();
                for part in blocks {
                    let id = json::string(&part["id"]);
                    match part["type"].as_str() {
                        Some("text") if !discarded && part["ignored"] != true => {
                            if let Some(body) = part["text"].as_str().and_then(normalize::assistant)
                            {
                                entries[index].push(draft(body, id));
                            }
                        }
                        Some("reasoning") if !discarded => {
                            if let Some(text) = json::string(&part["text"]) {
                                entries[index].push(draft(
                                    Body::Thinking(Thinking {
                                        text,
                                        signature: None,
                                    }),
                                    id,
                                ));
                            }
                        }
                        Some("tool") => {
                            let state = &part["state"];
                            if tool_order::synthetic_call_id(state).is_none() {
                                entries[index].push(draft(
                                    Body::ToolCall(ToolCall {
                                        call_id: normalize::call_id(&part["callID"])?,
                                        name: part["tool"].as_str().unwrap_or("unknown").to_owned(),
                                        input: normalize::tool_input(state["input"].clone()),
                                    }),
                                    id.clone(),
                                ));
                            }
                            if let Some(body) = tool_result(blobs, artifacts, part)? {
                                results.push(draft(body, id));
                            }
                        }
                        _ => {}
                    }
                }
                entries[index].append(&mut results);
                if !entries[index].is_empty()
                    && let (Some(provider), Some(id)) = (
                        message.data["providerId"].as_str(),
                        message.data["modelId"].as_str(),
                    )
                    && !provider.is_empty()
                    && !id.is_empty()
                    && model.as_ref().is_none_or(
                        |(previous_provider, previous_id): &(String, String)| {
                            previous_provider != provider || previous_id != id
                        },
                    )
                {
                    entries[index].insert(
                        0,
                        draft(
                            Body::Meta(json!({"type": "model", "provider": provider, "model": id})),
                            Some(message.id.clone()),
                        ),
                    );
                    model = Some((provider.to_owned(), id.to_owned()));
                }
            }
            _ => {}
        }
    }
    Ok(prefixes
        .into_iter()
        .zip(entries)
        .flat_map(|(prefix, entry)| prefix.into_iter().chain(entry))
        .collect())
}

fn active_branch<'a>(messages: &'a [Message], revert: Option<&Value>) -> Vec<&'a Message> {
    let Some(revert) = revert else {
        return messages.iter().collect();
    };
    let Some(target) = revert["targetMessageID"]
        .as_str()
        .filter(|target| !target.is_empty())
    else {
        return messages.iter().collect();
    };
    let target_index = messages.iter().position(|message| message.id == target);
    let kept_ids = revert["keptMessageIDs"].as_array();
    let mut kept: Vec<&Message> = match kept_ids {
        Some(ids) => {
            let by_id: HashMap<&str, &Message> = messages
                .iter()
                .map(|message| (message.id.as_str(), message))
                .collect();
            ids.iter()
                .filter_map(|id| by_id.get(id.as_str()?).copied())
                .collect()
        }
        None => messages[..target_index.unwrap_or(messages.len())]
            .iter()
            .collect(),
    };
    if let Some(cut) = revert["branchCutAfterMessageID"]
        .as_str()
        .filter(|cut| !cut.is_empty())
    {
        if let Some(index) = messages.iter().position(|message| message.id == cut) {
            kept.extend(&messages[index + 1..]);
        }
        return kept;
    }
    if kept_ids.is_none() && target_index.is_none() {
        return messages.iter().collect();
    }
    if let Some(created) = revert["createdMessageID"]
        .as_str()
        .filter(|created| !created.is_empty())
        && let Some(index) = messages.iter().position(|message| message.id == created)
    {
        kept.extend(&messages[index..]);
    }
    kept
}

fn text(parts: &[Value], include_synthetic: bool) -> String {
    parts
        .iter()
        .filter(|part| {
            part["type"] == "text"
                && part["ignored"] != true
                && (include_synthetic || part["synthetic"] != true)
        })
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result(blobs: &Blobs, artifacts: &Path, part: &Value) -> Result<Option<Body>> {
    let state = &part["state"];
    let mut attached = Vec::new();
    let (text, is_error) = match state["status"].as_str() {
        Some("completed") => {
            if let Some(attachments) = state["attachments"].as_array() {
                for attachment in attachments {
                    attached.extend(file_image(blobs, artifacts, attachment)?);
                }
            }
            (state["output"].as_str(), false)
        }
        Some("error") => {
            if let Some(urls) = state["metadata"]["baton"]["images"].as_array() {
                for url in urls {
                    if let Some(url) = url.as_str() {
                        attached.extend(images::from_data_url(blobs, url)?);
                    }
                }
            }
            (
                state["metadata"]["modelContent"]
                    .as_str()
                    .or_else(|| state["error"].as_str()),
                true,
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(Body::ToolResult(ToolResult {
        call_id: tool_order::synthetic_call_id(state).map_or_else(
            || normalize::call_id(&part["callID"]),
            |id| Ok(id.to_owned()),
        )?,
        text: text.unwrap_or_default().to_owned(),
        images: attached,
        is_error,
    })))
}

fn file_image(blobs: &Blobs, artifacts: &Path, part: &Value) -> Result<Option<Image>> {
    if part["type"] != "file"
        || !part["mime"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("image/"))
    {
        return Ok(None);
    }
    if let Some(url) = part["url"].as_str().filter(|url| url.starts_with("data:")) {
        return images::from_data_url(blobs, url);
    }
    let uri = [
        part["metadata"]["artifactUri"].as_str(),
        part["url"].as_str(),
    ]
    .into_iter()
    .flatten()
    .find_map(|url| url.strip_prefix("zcode-artifact://"));
    let Some((session, artifact)) = uri.and_then(|uri| uri.split_once('/')) else {
        return Ok(None);
    };
    let (Some(session), Some(artifact)) = (percent_decode(session), percent_decode(artifact))
    else {
        return Ok(None);
    };
    if session.is_empty() || artifact.is_empty() {
        return Ok(None);
    }
    let directory: String = session
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let directory = artifacts.join(directory);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", directory.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().contains(&artifact) {
            let path = entry.path();
            let url =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            return images::from_data_url(blobs, &url);
        }
    }
    Ok(None)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}
