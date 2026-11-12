use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};
use baton_core::journal::{Line, Node, Parser};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Info, Thinking, ToolCall, ToolResult, time,
};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Reads the main agent's journal of a Kimi Code session, a flat stream whose order is the conversation
pub(super) struct WireParser<'a> {
    blobs: &'a Blobs,
    // Kimi keeps large images as files here, referred to as `blobref:<mime>;<hash>`
    blob_dir: PathBuf,
    // Whether the parser continues an earlier read, which never saw the turns before the cursor
    continuing: bool,
    last: Option<String>,
    model: Option<String>,
    // The node before each user turn, so an undo can return to it
    turns: Vec<(Option<String>, Option<String>)>,
}

impl<'a> WireParser<'a> {
    pub(super) fn new(blobs: &'a Blobs, blob_dir: PathBuf, leaf: Option<&str>) -> WireParser<'a> {
        WireParser {
            blobs,
            blob_dir,
            continuing: leaf.is_some(),
            last: leaf.map(String::from),
            model: None,
            turns: Vec::new(),
        }
    }

    fn bodies(&mut self, record: &Value) -> Result<Vec<Body>> {
        let bodies = match record["type"].as_str().unwrap_or_default() {
            "profile.bind" | "config.update" => {
                if let Some(model) = record["modelAlias"]
                    .as_str()
                    .filter(|model| !model.is_empty())
                    && self.model.as_deref() != Some(model)
                {
                    self.model = Some(model.to_owned());
                    vec![Body::Meta(json!({"type": "model", "model": model}))]
                } else {
                    Vec::new()
                }
            }
            "context.append_message" => self.message(&record["message"])?,
            "context.append_loop_event" => self.loop_event(&record["event"])?,
            "context.apply_compaction" => {
                // An undo never reaches back across a compaction
                self.turns.clear();
                // Older journals stored the summary as a message
                let summary = json::string(&record["summary"])
                    .or_else(|| json::string(&record["contextSummary"]))
                    .unwrap_or_else(|| {
                        json::texts(parts(&record["summary"]["content"]), &["text"])
                    });
                vec![if summary.is_empty() {
                    Body::Meta(json!({"type": "compaction"}))
                } else {
                    Body::Compaction(Compaction { summary })
                }]
            }
            "context.clear" => vec![Body::Meta(json!({"type": "reset"}))],
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn message(&mut self, message: &Value) -> Result<Vec<Body>> {
        let bodies = match message["role"].as_str().unwrap_or_default() {
            // Other origins are messages the runtime injected
            "user" if typed(&message["origin"]) => {
                self.turns.push((self.last.clone(), self.model.clone()));
                normalize::user(self.content(&message["content"])?)
                    .into_iter()
                    .collect()
            }
            "assistant" => {
                let mut bodies = Vec::new();
                for part in parts(&message["content"]) {
                    bodies.extend(assistant_part(part));
                }
                for call in parts(&message["toolCalls"]) {
                    bodies.push(Body::ToolCall(ToolCall {
                        call_id: normalize::call_id(&call["id"])?,
                        name: call["name"].as_str().unwrap_or("unknown").to_owned(),
                        input: normalize::arguments(&call["arguments"]),
                    }));
                }
                bodies
            }
            "tool" => {
                let content = self.content(&message["content"])?;
                vec![result(
                    &message["toolCallId"],
                    content,
                    &message["note"],
                    message["isError"] == true,
                )?]
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn loop_event(&self, event: &Value) -> Result<Vec<Body>> {
        let bodies = match event["type"].as_str().unwrap_or_default() {
            "content.part" => assistant_part(&event["part"]).into_iter().collect(),
            "tool.call" => vec![Body::ToolCall(ToolCall {
                call_id: normalize::call_id(&event["toolCallId"])?,
                name: event["name"].as_str().unwrap_or("unknown").to_owned(),
                input: normalize::tool_input(event["args"].clone()),
            })],
            "tool.result" => {
                let output = &event["result"]["output"];
                let content = match output.as_str() {
                    Some(text) => Content {
                        text: text.to_owned(),
                        images: Vec::new(),
                    },
                    None => self.content(output)?,
                };
                let note = &event["result"]["note"];
                vec![result(
                    &event["toolCallId"],
                    content,
                    note,
                    event["result"]["isError"] == true,
                )?]
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn content(&self, content: &Value) -> Result<Content> {
        let parts = parts(content);
        let mut attached = Vec::new();
        for part in parts {
            if part["type"] == "image_url" {
                attached.extend(self.image(part["imageUrl"]["url"].as_str().unwrap_or_default())?);
            }
        }
        Ok(Content {
            text: json::texts(parts, &["text"]),
            images: attached,
        })
    }

    fn image(&self, url: &str) -> Result<Option<Image>> {
        let Some((mime, hash)) = url
            .strip_prefix("blobref:")
            .and_then(|reference| reference.split_once(';'))
        else {
            return images::from_data_url(self.blobs, url);
        };
        if hash.is_empty()
            || matches!(hash, "." | "..")
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Ok(None);
        }
        let path = self.blob_dir.join(hash);
        match fs::read(&path) {
            Ok(bytes) => images::store(self.blobs, mime, &bytes).map(Some),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    // Removes the last `count` user turns with everything after them, as Kimi's undo does
    fn undo(&mut self, count: u64) -> Line {
        for _ in 0..count {
            match self.turns.pop() {
                Some((before, model)) => {
                    self.last = before;
                    self.model = model;
                }
                // An incremental read never saw the turns it would have to undo
                None if self.continuing => return Line::Restart,
                None => break,
            }
        }
        Line::Skip
    }
}

// Messages the user typed, including slash commands they ran
fn typed(origin: &Value) -> bool {
    match origin["kind"].as_str() {
        None | Some("user") => true,
        Some("skill_activation" | "plugin_command") => origin["trigger"] == "user-slash",
        Some(_) => false,
    }
}

fn parts(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn assistant_part(part: &Value) -> Option<Body> {
    match part["type"].as_str()? {
        "text" => normalize::assistant(part["text"].as_str()?),
        "think" => Some(Body::Thinking(Thinking {
            text: part["think"].as_str().unwrap_or_default().to_owned(),
            signature: json::string(&part["encrypted"]),
        })),
        _ => None,
    }
}

// The model sees a tool's note after its output
fn result(call_id: &Value, mut content: Content, note: &Value, is_error: bool) -> Result<Body> {
    if let Some(note) = json::string(note) {
        content.text.push('\n');
        content.text.push_str(&note);
    }
    Ok(Body::ToolResult(ToolResult {
        call_id: normalize::call_id(call_id)?,
        text: content.text,
        images: content.images,
        is_error,
    }))
}

impl Parser for WireParser<'_> {
    fn line(&mut self, offset: u64, record: Value) -> Result<Line> {
        if record["type"] == "context.undo" {
            return Ok(self.undo(record["count"].as_u64().unwrap_or(1)));
        }
        let bodies = self.bodies(&record)?;
        if bodies.is_empty() {
            return Ok(Line::Skip);
        }
        let timestamp = record["time"]
            .as_i64()
            .and_then(time::from_unix_millis)
            .unwrap_or_else(time::now);
        // Records have no ids, so their byte offsets stand in
        let id = offset.to_string();
        let parent = self.last.replace(id.clone());
        let native_entry_id = json::string(&record["message"]["id"])
            .or_else(|| json::string(&record["event"]["uuid"]));
        let mut drafts = Vec::with_capacity(bodies.len());
        for body in bodies {
            drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: native_entry_id.clone(),
                body,
            });
        }
        Ok(Line::Node(Node {
            id,
            parent,
            drafts,
            anchor: None,
        }))
    }

    // The session facts live in `state.json`, which the adapter reads
    fn finish(self) -> Info {
        Info::default()
    }
}
