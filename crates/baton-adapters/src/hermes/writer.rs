use anyhow::Result;
use baton_core::{Blobs, Event, Image, time};
use serde_json::{Value, json};

use crate::hermes::parser::{LEGACY_SUMMARY, STRUCTURED};
use crate::support::images;
use crate::support::unit::{self, Unit};

// A `messages` row as baton inserts it
pub(super) struct Row {
    pub(super) role: &'static str,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_calls: Vec<Value>,
    pub(super) tool_name: Option<String>,
    pub(super) timestamp: f64,
    pub(super) summary: bool,
}

impl Row {
    fn new(role: &'static str, content: String, event: &Event) -> Row {
        Row {
            role,
            content,
            tool_call_id: None,
            tool_calls: Vec::new(),
            tool_name: None,
            timestamp: time::unix_millis(&event.timestamp) as f64 / 1000.0,
            summary: false,
        }
    }
}

// Hermes merges consecutive assistant rows itself before sending them, so splitting a turn into replies only keeps the stored order exact
pub(super) fn rows(events: &[&Event], blobs: &Blobs) -> Result<Vec<Row>> {
    let calls = unit::calls_by_id(events);
    let mut rows = Vec::new();
    for unit in unit::units(events) {
        match unit {
            Unit::User(event, user) => {
                let content = content(&user.text, &user.images, blobs)?;
                rows.push(Row::new("user", content, event));
            }
            Unit::Assistant(run) => {
                for reply in unit::replies(&run) {
                    let mut row = Row::new("assistant", reply.text.to_owned(), reply.event);
                    for call in reply.calls {
                        row.tool_calls.push(unit::encode_function_call(call));
                    }
                    rows.push(row);
                }
            }
            Unit::ToolResult(event, result) => {
                let content = content(&result.text, &result.images, blobs)?;
                let mut row = Row::new("tool", content, event);
                row.tool_call_id = Some(result.call_id.clone());
                let name = calls
                    .get(result.call_id.as_str())
                    .map(|call| call.name.as_str());
                row.tool_name = Some(name.unwrap_or("unknown_tool").to_owned());
                rows.push(row);
            }
            Unit::Compaction(event, summary) => {
                let mut row = Row::new("user", format!("{LEGACY_SUMMARY}\n{summary}"), event);
                row.summary = true;
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

fn content(text: &str, attached: &[Image], blobs: &Blobs) -> Result<String> {
    if attached.is_empty() {
        return Ok(text.to_owned());
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    for image in attached {
        let url = images::data_url(blobs, image)?;
        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
    }
    Ok(format!("{STRUCTURED}{}", Value::Array(parts)))
}
