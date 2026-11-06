use std::collections::HashMap;
use std::mem;

use anyhow::Result;
use baton_core::{Blobs, Event, ToolCall, ToolResult};
use serde_json::{Value, json};

use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images};
use crate::vibe::message_parser::SYNTHETIC_PREFIX;

// What Vibe itself answers a call with when it got no result
const NO_RESPONSE: &str =
    "<user_cancellation>Tool execution interrupted - no response available</user_cancellation>";

// Each line is written the way Vibe dumps it again, every default included, so its fingerprint stays valid
pub(super) fn messages(events: &[&Event], blobs: &Blobs) -> Result<Vec<Value>> {
    let mut writer = MessageWriter {
        calls: unit::calls_by_id(events),
        lines: Vec::new(),
        pending: Vec::new(),
    };
    for unit in unit::units(events) {
        match unit {
            Unit::User(_, user) => {
                writer.close();
                let mut line = json!({"role": "user", "content": user.text, "injected": false, "message_id": identifiers::uuid_v4()});
                if !user.images.is_empty() {
                    let mut attached = Vec::with_capacity(user.images.len());
                    for (index, image) in user.images.iter().enumerate() {
                        let data = images::encode(&blobs.get(&image.blob)?);
                        let alias =
                            format!("image-{}.{}", index + 1, images::extension(&image.mime));
                        attached.push(json!({"source": {"kind": "inline", "data": data}, "alias": alias, "mime_type": image.mime}));
                    }
                    line["images"] = json!(attached);
                }
                writer.lines.push(line);
            }
            Unit::Assistant(run) => {
                for reply in unit::replies(&run) {
                    writer.close();
                    writer.push_calls(reply.text, &reply.calls, identifiers::uuid_v4());
                    writer.pending = reply
                        .calls
                        .iter()
                        .map(|call| call.call_id.as_str())
                        .collect();
                }
            }
            Unit::ToolResult(_, result) => writer.push_result(result, blobs)?,
            Unit::Compaction(_, summary) => {
                writer.close();
                writer.lines.push(json!({"role": "user", "content": summary, "injected": true,
                                         "message_id": identifiers::uuid_v4(), "context_boundary": "compaction"}));
            }
        }
    }
    writer.close();

    Ok(writer.lines)
}

// Vibe needs the results of an assistant line's calls on the lines right after it, so where baton's order differs it gets marked lines the reader drops
struct MessageWriter<'a> {
    calls: HashMap<&'a str, &'a ToolCall>,
    lines: Vec<Value>,
    // Calls of the last assistant line that have no result yet
    pending: Vec<&'a str>,
}

impl<'a> MessageWriter<'a> {
    fn push_calls(&mut self, text: &str, calls: &[&ToolCall], message_id: String) {
        let mut line = json!({"role": "assistant", "content": text, "injected": false, "message_id": message_id});
        if !calls.is_empty() {
            let calls: Vec<Value> = calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let mut object = unit::encode_function_call(call);
                    object["index"] = json!(index);
                    object
                })
                .collect();
            line["tool_calls"] = json!(calls);
        }
        self.lines.push(line);
    }

    fn push_result(&mut self, result: &'a ToolResult, blobs: &Blobs) -> Result<()> {
        if let Some(position) = self.pending.iter().position(|id| *id == result.call_id) {
            self.pending.remove(position);
        } else {
            // A result away from its call answers a marked call of its own
            self.close();
            let call = self.calls.get(result.call_id.as_str());
            let synthetic = ToolCall {
                call_id: result.call_id.clone(),
                name: call.map_or_else(|| "unknown".into(), |call| call.name.clone()),
                input: call.map_or_else(|| json!({}), |call| call.input.clone()),
            };
            self.push_calls(
                "",
                &[&synthetic],
                format!("{SYNTHETIC_PREFIX}{}", identifiers::uuid_v4()),
            );
        }
        let mut marker = json!({"is_error": result.is_error});
        if !result.images.is_empty() {
            let mut attached = Vec::with_capacity(result.images.len());
            for image in &result.images {
                let data = images::encode(&blobs.get(&image.blob)?);
                attached.push(json!({"mime_type": image.mime, "data": data}));
            }
            marker["images"] = json!(attached);
        }
        let content = if result.is_error {
            format!("<tool_error>{}</tool_error>", result.text)
        } else {
            result.text.clone()
        };
        let line = self.tool_line(
            &result.call_id,
            content,
            json!({"output": {"baton": marker}, "cancelled": false}),
        );
        self.lines.push(line);
        Ok(())
    }

    // Answers the calls still waiting, which is what Vibe would do before the next turn
    fn close(&mut self) {
        for id in mem::take(&mut self.pending) {
            let line = self.tool_line(
                id,
                NO_RESPONSE.to_owned(),
                json!({"output": {"baton": {"synthetic": true}}, "cancelled": true}),
            );
            self.lines.push(line);
        }
    }

    fn tool_line(&self, id: &str, content: String, result: Value) -> Value {
        let name = self
            .calls
            .get(id)
            .map_or("unknown", |call| call.name.as_str());
        json!({"role": "tool", "content": content, "injected": false, "name": name, "tool_call_id": id, "tool_result": result})
    }
}
