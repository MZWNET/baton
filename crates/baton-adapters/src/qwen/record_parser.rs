use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolCall, ToolResult, time,
};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Hook output Qwen appends to a prompt as its last text part
const HOOK_CONTEXT_OPEN: &str = "<qwen:user-prompt-submit-context>\n";
const HOOK_CONTEXT_CLOSE: &str = "\n</qwen:user-prompt-submit-context>";

// Reads the records of a Qwen Code chat, a tree linked through `parentUuid`
pub(super) struct RecordParser<'a> {
    blobs: &'a Blobs,
    model: Option<String>,
    info: Info,
}

impl<'a> RecordParser<'a> {
    pub(super) fn new(blobs: &'a Blobs) -> RecordParser<'a> {
        RecordParser {
            blobs,
            model: None,
            info: Info::default(),
        }
    }

    fn bodies(&mut self, record: &Value) -> Result<Vec<Body>> {
        let parts = record["message"]["parts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let subtype = record["subtype"].as_str();
        let bodies = match (record["type"].as_str().unwrap_or_default(), subtype) {
            // Other subtypes are notifications and reminders the runtime injected
            ("user", None | Some("mid_turn_user_message")) => {
                let parts = match parts.split_last() {
                    Some((last, rest)) if !rest.is_empty() && is_hook_context(last) => rest,
                    _ => parts,
                };
                normalize::user(self.content(parts)?).into_iter().collect()
            }
            ("assistant", subtype) if subtype != Some("realtime_message") => {
                let mut bodies = Vec::new();
                if let Some(model) = json::string(&record["model"])
                    && self.model.as_ref() != Some(&model)
                {
                    bodies.push(Body::Meta(json!({"type": "model", "model": model})));
                    self.model = Some(model);
                }
                for part in parts {
                    bodies.extend(assistant_part(part));
                }
                bodies
            }
            ("tool_result", _) => self.results(record, parts)?,
            ("system", Some("chat_compression")) => compaction(&record["systemPayload"]),
            ("system", Some("custom_title")) => {
                self.info.title = json::string(&record["systemPayload"]["customTitle"])
                    .or(self.info.title.take());
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn content(&self, parts: &[Value]) -> Result<Content> {
        let mut attached = Vec::new();
        for part in parts {
            attached.extend(self.image(part)?);
        }
        Ok(Content {
            text: texts(parts),
            images: attached,
        })
    }

    fn image(&self, part: &Value) -> Result<Option<Image>> {
        let inline = &part["inlineData"];
        match (
            inline["mimeType"]
                .as_str()
                .filter(|mime| mime.starts_with("image/")),
            inline["data"].as_str().and_then(images::decode),
        ) {
            (Some(mime), Some(bytes)) => images::store(self.blobs, mime, &bytes).map(Some),
            _ => Ok(None),
        }
    }

    // A record holds one result per function response; older files put a result's images in sibling parts
    fn results(&self, record: &Value, parts: &[Value]) -> Result<Vec<Body>> {
        let status = record["toolCallResult"]["status"].as_str();
        let mut results: Vec<ToolResult> = Vec::new();
        for part in parts {
            let response = &part["functionResponse"];
            if response.is_null() {
                if let Some(result) = results.last_mut() {
                    result.images.extend(self.image(part)?);
                }
                continue;
            }
            let reply = &response["response"];
            let mut text = match (&reply["output"], &reply["error"]) {
                (Value::String(output), _) => output.clone(),
                (_, Value::String(error)) => error.clone(),
                _ if reply.is_null()
                    || reply.as_object().is_some_and(|fields| fields.is_empty()) =>
                {
                    String::new()
                }
                _ => reply.to_string(),
            };
            let nested = response["parts"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut attached = Vec::new();
            for nested in nested {
                attached.extend(self.image(nested)?);
                if let Some(extra) = json::string(&nested["text"]) {
                    text.push('\n');
                    text.push_str(&extra);
                }
            }
            let is_error = match status {
                Some(status) => status == "error" || status == "cancelled",
                None => reply["error"].is_string() && reply["output"].is_null(),
            };
            results.push(ToolResult {
                call_id: normalize::call_id(&response["id"]),
                text,
                images: attached,
                is_error,
            });
        }
        Ok(results.into_iter().map(Body::ToolResult).collect())
    }
}

fn is_hook_context(part: &Value) -> bool {
    part["text"].as_str().is_some_and(|text| {
        text.starts_with(HOOK_CONTEXT_OPEN) && text.ends_with(HOOK_CONTEXT_CLOSE)
    })
}

fn texts(parts: &[Value]) -> String {
    let mut texts = Vec::new();
    for part in parts {
        if part["thought"] != true
            && let Some(text) = part["text"].as_str()
        {
            texts.push(text);
        }
    }
    texts.join("\n")
}

fn assistant_part(part: &Value) -> Option<Body> {
    if let Some(text) = part["text"].as_str() {
        if part["thought"] == true {
            return Some(Body::Thinking(Thinking {
                text: text.to_owned(),
                signature: json::string(&part["thoughtSignature"]),
            }));
        }
        return normalize::assistant(text);
    }
    let call = &part["functionCall"];
    if call.is_null() {
        return None;
    }
    Some(Body::ToolCall(ToolCall {
        call_id: normalize::call_id(&call["id"]),
        name: call["name"].as_str().unwrap_or("unknown").to_owned(),
        input: normalize::tool_input(call["args"].clone()),
    }))
}

// Qwen resumes a compressed chat from its compressed history: the summary as a user turn, then the model's reply and the calls it still waits on
fn compaction(payload: &Value) -> Vec<Body> {
    let history = payload["compressedHistory"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let summary = history
        .iter()
        .find(|content| content["role"] == "user")
        .map(|content| {
            texts(
                content["parts"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    let mut bodies = vec![if summary.is_empty() {
        Body::Meta(json!({"type": "compaction"}))
    } else {
        Body::Compaction(Compaction { summary })
    }];
    for content in history.iter().filter(|content| content["role"] == "model") {
        let parts = content["parts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for part in parts {
            if !part["functionCall"].is_null() {
                bodies.extend(assistant_part(part));
            }
        }
    }
    bodies
}

impl Parser for RecordParser<'_> {
    fn line(&mut self, _offset: u64, record: Value) -> Result<Line> {
        // Artifact records hang off the tail without becoming it
        if matches!(
            record["subtype"].as_str(),
            Some("session_artifact_event" | "session_artifact_snapshot")
        ) {
            return Ok(Line::Skip);
        }
        let Some(id) = json::string(&record["uuid"]) else {
            return Ok(Line::Skip);
        };
        if self.info.cwd.is_none() {
            self.info.cwd = json::string(&record["cwd"]);
        }
        let fallback = self.info.created.clone().unwrap_or_else(time::now);
        let timestamp = json::timestamp(&record["timestamp"], &fallback);
        if self.info.created.is_none() {
            self.info.created = Some(timestamp.clone());
        }
        let mut drafts = Vec::new();
        for body in self.bodies(&record)? {
            drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: Some(id.clone()),
                body,
            });
        }
        Ok(Line::Node(Node {
            parent: json::string(&record["parentUuid"]),
            id,
            drafts,
            anchor: None,
        }))
    }

    fn finish(self) -> Info {
        self.info
    }
}
