use anyhow::Result;
use baton_core::{Blobs, Body, Compaction, Draft, Thinking, ToolResult, time};
use serde_json::Value;

use crate::support::normalize::{self, Content};
use crate::support::{images, json, unit};

// Structured content is JSON behind this prefix; anything else is plain text
pub(super) const STRUCTURED: &str = "\u{0}json:";
// What baton writes summaries with, and what Hermes wrote before its compaction prefix
pub(super) const LEGACY_SUMMARY: &str = "[CONTEXT SUMMARY]:";
// Hermes's summary prefix is one line whose wording changes between versions
const SUMMARY_PREFIX: &str = "[CONTEXT COMPACTION";
const SUMMARY_END: &str =
    "--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---";
const PRIOR_CONTEXT_END: &str = "[END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]";
const SNAPSHOT: &str = "\n## Historical Task Snapshot\n";

// A `messages` row with its reasoning columns reduced to readable text and opaque details
pub(super) struct Row {
    pub(super) id: i64,
    pub(super) role: String,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_calls: Option<String>,
    pub(super) effect_disposition: Option<String>,
    pub(super) timestamp: f64,
    pub(super) reasoning: Option<String>,
    pub(super) reasoning_details: Option<String>,
    pub(super) summary: bool,
}

pub(super) fn drafts(row: &Row, blobs: &Blobs) -> Result<Vec<Draft>> {
    let timestamp =
        time::from_unix_millis((row.timestamp * 1000.0).round() as i64).unwrap_or_else(time::now);
    let content = content(&row.content, blobs)?;
    let bodies = if row.summary {
        summary_bodies(row, content)?
    } else {
        bodies(row, content)?
    };
    let mut drafts = Vec::with_capacity(bodies.len());
    for body in bodies {
        drafts.push(Draft {
            timestamp: timestamp.clone(),
            native_entry_id: Some(row.id.to_string()),
            body,
        });
    }
    Ok(drafts)
}

fn bodies(row: &Row, content: Content) -> Result<Vec<Body>> {
    let bodies = match row.role.as_str() {
        "user" => normalize::user(content).into_iter().collect(),
        "assistant" => {
            let mut bodies = Vec::new();
            let thinking = Thinking {
                text: row.reasoning.clone().unwrap_or_default(),
                signature: row.reasoning_details.clone(),
            };
            if !thinking.text.is_empty() || thinking.signature.is_some() {
                bodies.push(Body::Thinking(thinking));
            }
            bodies.extend(normalize::assistant(&content.text));
            bodies.extend(tool_calls(row.tool_calls.as_deref())?);
            bodies
        }
        "tool" => vec![Body::ToolResult(ToolResult {
            call_id: normalize::call_id(&Value::from(row.tool_call_id.as_deref()))?,
            is_error: failed(row, &content.text),
            text: content.text,
            images: content.images,
        })],
        _ => Vec::new(),
    };
    Ok(bodies)
}

fn content(text: &str, blobs: &Blobs) -> Result<Content> {
    let Some(structured) = text.strip_prefix(STRUCTURED) else {
        return Ok(Content {
            text: text.to_owned(),
            images: Vec::new(),
        });
    };
    let parts = match serde_json::from_str(structured) {
        Ok(Value::Array(parts)) => parts,
        Ok(part) => vec![part],
        Err(_) => Vec::new(),
    };
    let mut attached = Vec::new();
    for part in &parts {
        let url = match &part["image_url"] {
            Value::String(url) => url.as_str(),
            image_url => image_url["url"].as_str().unwrap_or_default(),
        };
        attached.extend(images::from_data_url(blobs, url)?);
    }
    Ok(Content {
        text: json::texts(&parts, &["text", "input_text"]),
        images: attached,
    })
}

fn tool_calls(text: Option<&str>) -> Result<Vec<Body>> {
    let Some(Value::Array(calls)) = text.and_then(|text| serde_json::from_str(text).ok()) else {
        return Ok(Vec::new());
    };
    let mut bodies = Vec::with_capacity(calls.len());
    for call in &calls {
        let id = if call["id"].is_string() {
            &call["id"]
        } else {
            &call["call_id"]
        };
        bodies.push(Body::ToolCall(unit::decode_function_call(call, id)?));
    }
    Ok(bodies)
}

// Hermes has no error column; failures show in the tool's JSON envelope or in the effect disposition
fn failed(row: &Row, text: &str) -> bool {
    let disposition = row.effect_disposition.as_deref().unwrap_or_default();
    if ["fail", "error", "deny"]
        .iter()
        .any(|word| disposition.contains(word))
    {
        return true;
    }
    let Ok(Value::Object(envelope)) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let error = envelope
        .get("error")
        .is_some_and(|error| !error.is_null() && error != "");
    let exit = envelope
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0);
    error || exit
}

// A summary row in any of the shapes Hermes has written, or the legacy shape baton writes
fn summary_bodies(row: &Row, content: Content) -> Result<Vec<Body>> {
    let text = content.text.as_str();
    if let Some(summary) = text.strip_prefix(LEGACY_SUMMARY) {
        return Ok(compaction(summary.strip_prefix('\n').unwrap_or(summary))
            .into_iter()
            .collect());
    }
    // A summary merged into a kept row follows that row's own content, which stays as it was
    if let Some((before, after)) = text.split_once(PRIOR_CONTEXT_END) {
        let before = before.split_once('\n').map_or("", |(_, rest)| rest);
        let before = Content {
            text: before.strip_suffix("\n\n").unwrap_or(before).to_owned(),
            images: content.images,
        };
        let (summary, _) = prefixed(after.trim_start_matches('\n'));
        let mut bodies: Vec<Body> = compaction(summary).into_iter().collect();
        bodies.extend(self::bodies(row, before)?);
        return Ok(bodies);
    }
    if text.starts_with(SUMMARY_PREFIX) {
        // A micro-compaction snapshot stands in for one exchange, not for the history before it
        let snapshot = text
            .split_once('\n')
            .is_some_and(|(_, rest)| rest.starts_with(SNAPSHOT));
        if row.role == "assistant" && snapshot {
            return Ok(normalize::assistant(text).into_iter().collect());
        }
        let (summary, live) = prefixed(text);
        let mut bodies: Vec<Body> = compaction(summary).into_iter().collect();
        let live = Content {
            text: live.to_owned(),
            images: content.images,
        };
        bodies.extend(normalize::user(live));
        return Ok(bodies);
    }
    Ok(compaction(text).into_iter().collect())
}

// The summary after Hermes's prefix line, and the live message that may follow its end marker
fn prefixed(text: &str) -> (&str, &str) {
    let body = text.split_once('\n').map_or("", |(_, rest)| rest);
    match body.split_once(SUMMARY_END) {
        Some((summary, live)) => (
            summary.strip_suffix("\n\n").unwrap_or(summary),
            live.trim_start_matches('\n'),
        ),
        None => (body, ""),
    }
}

fn compaction(summary: &str) -> Option<Body> {
    (!summary.trim().is_empty()).then(|| {
        Body::Compaction(Compaction {
            summary: summary.to_owned(),
        })
    })
}
