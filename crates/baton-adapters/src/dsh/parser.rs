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

pub(super) const SYNTHETIC_PREFIX: &str = "baton-synthetic-";
pub(super) const BATON_PROVIDER: &str = "baton";

pub(super) struct EventParser<'a> {
    blobs: &'a Blobs,
    objects_dir: PathBuf,
    last: Option<String>,
    model: Option<String>,
    info: Info,
}

impl<'a> EventParser<'a> {
    pub(super) fn new(blobs: &'a Blobs, objects_dir: PathBuf, leaf: Option<&str>) -> Self {
        Self {
            blobs,
            objects_dir,
            last: leaf.map(String::from),
            model: None,
            info: Info::default(),
        }
    }

    fn bodies(&mut self, event: &Value) -> Result<Vec<Body>> {
        let data = &event["data"];
        let bodies = match event["type"].as_str().unwrap_or_default() {
            "user/message" => match data["source"]["kind"].as_str() {
                Some("user") => normalize::user(self.content(&data["content"])?)
                    .into_iter()
                    .collect(),
                Some("compact-checkpoint") => checkpoint(&data["content"])
                    .map(|summary| Body::Compaction(Compaction { summary }))
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            },
            "assistant/message" => self.assistant(&data["message"]),
            "tool/result"
                if event["surfaceOp"] == "append"
                    && data["error"]["code"] != "TOOL_NOT_STARTED" =>
            {
                let message = &data["message"];
                let content = self.content(&message["content"])?;
                vec![Body::ToolResult(ToolResult {
                    call_id: normalize::call_id(&message["toolCallId"]),
                    text: content.text,
                    images: content.images,
                    is_error: message["isError"] == true,
                })]
            }
            "session/title" => {
                self.info.title = data["title"].as_str().map(String::from);
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn assistant(&mut self, message: &Value) -> Vec<Body> {
        if message["id"]
            .as_str()
            .is_some_and(|id| id.starts_with(SYNTHETIC_PREFIX))
        {
            return Vec::new();
        }
        let mut bodies = Vec::new();
        if message["source"]["provider"] != BATON_PROVIDER
            && let Some(model) = json::string(&message["source"]["model"])
            && self.model.as_ref() != Some(&model)
        {
            bodies.push(Body::Meta(json!({"type": "model", "model": model})));
            self.model = Some(model);
        }
        for part in parts(&message["content"]) {
            let body = match part["type"].as_str() {
                Some("reasoning") => Some(Body::Thinking(Thinking {
                    text: part["text"].as_str().unwrap_or_default().to_owned(),
                    signature: None,
                })),
                Some("text") => normalize::assistant(part["text"].as_str().unwrap_or_default()),
                Some("tool-call") => Some(Body::ToolCall(ToolCall {
                    call_id: normalize::call_id(&part["id"]),
                    name: part["name"].as_str().unwrap_or("unknown").to_owned(),
                    input: normalize::arguments(&part["arguments"]),
                })),
                _ => None,
            };
            bodies.extend(body);
        }
        bodies
    }

    fn content(&self, value: &Value) -> Result<Content> {
        let parts = parts(value);
        let mut attached = Vec::new();
        for part in parts {
            if part["type"] == "image" {
                attached.extend(self.image(&part["attachment"])?);
            }
        }
        Ok(Content {
            text: json::texts(parts, &["text"]),
            images: attached,
        })
    }

    fn image(&self, attachment: &Value) -> Result<Option<Image>> {
        let Some(hash) = attachment["attachmentId"]
            .as_str()
            .and_then(|id| id.strip_prefix("sha256:"))
        else {
            return Ok(None);
        };
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Ok(None);
        }
        let Some(mime) = attachment["mediaType"].as_str() else {
            return Ok(None);
        };
        let path = self.objects_dir.join(&hash[..2]).join(hash);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        images::store(self.blobs, mime, &bytes).map(Some)
    }
}

fn parts(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn checkpoint(content: &Value) -> Option<String> {
    let parts = parts(content);
    let start = parts.iter().position(|part| {
        part["type"] == "text"
            && part["text"]
                .as_str()
                .is_some_and(|text| text.ends_with("<compacted-summary>"))
    })?;
    let summary = &parts[start + 1..];
    let end = summary
        .iter()
        .position(|part| part["type"] == "text" && part["text"] == "</compacted-summary>")?;
    Some(json::texts(&summary[..end], &["text"]))
}

impl Parser for EventParser<'_> {
    fn line(&mut self, _offset: u64, event: Value) -> Result<Line> {
        if event["type"] == "session" {
            self.info.cwd = json::string(&event["cwd"]);
            self.info.created = Some(json::timestamp_from_millis(
                &event["createdAt"],
                &time::now(),
            ));
            return Ok(Line::Skip);
        }
        let Some(seq) = event["seq"].as_u64() else {
            return Ok(Line::Skip);
        };
        let native_entry_id = match event["type"].as_str() {
            Some("user/message") => json::string(&event["data"]["id"]),
            Some("assistant/message" | "tool/result") => {
                json::string(&event["data"]["message"]["id"])
            }
            _ => None,
        };
        let timestamp = json::timestamp_from_millis(&event["time"], &time::now());
        let drafts = self
            .bodies(&event)?
            .into_iter()
            .map(|body| Draft {
                timestamp: timestamp.clone(),
                native_entry_id: native_entry_id.clone(),
                body,
            })
            .collect();
        let id = seq.to_string();
        let parent = self.last.replace(id.clone());
        Ok(Line::Node(Node {
            id,
            parent,
            drafts,
            anchor: None,
        }))
    }

    fn finish(self) -> Info {
        self.info
    }
}
