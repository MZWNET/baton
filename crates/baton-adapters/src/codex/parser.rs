use std::path::Path;

use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{Blobs, Body, Compaction, Draft, Info, Thinking, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// Reads the records of a Codex rollout, a flat list whose order is the conversation
pub(super) struct RolloutParser<'a> {
    blobs: &'a Blobs,
    // Whether the parser continues an earlier read, which never saw the turns before the cursor
    continuing: bool,
    last: Option<String>,
    // The node before each user turn, so a rollback can return to it
    turns: Vec<Option<String>>,
    info: Info,
}

// A tool output, which only some Codex versions mark as failed
struct Output {
    content: Content,
    is_error: bool,
}

impl<'a> RolloutParser<'a> {
    pub(super) fn new(blobs: &'a Blobs, leaf: Option<&str>) -> RolloutParser<'a> {
        RolloutParser {
            blobs,
            continuing: leaf.is_some(),
            last: leaf.map(String::from),
            turns: Vec::new(),
            info: Info::default(),
        }
    }

    // Legacy rollouts replay response items; paginated ones, with ordinals, keep the conversation in completed turn items
    fn bodies(&mut self, record: &Value) -> Result<Vec<Body>> {
        let payload = &record["payload"];
        let paginated = record.get("ordinal").is_some();
        let bodies = match record["type"].as_str().unwrap_or_default() {
            "response_item" => self.response_item(payload, paginated)?,
            "event_msg" => match payload["type"].as_str().unwrap_or_default() {
                "user_message" if !paginated => self.user_message(payload)?,
                "item_completed" if paginated => self.completed_item(&payload["item"])?,
                _ => Vec::new(),
            },
            "compacted" => match json::string(&payload["message"]) {
                Some(summary) => vec![Body::Compaction(Compaction { summary })],
                // Remote compaction keeps only encrypted state, so the visible history stays in force
                None => vec![Body::Meta(json!({"type": "compaction"}))],
            },
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn user_message(&mut self, payload: &Value) -> Result<Vec<Body>> {
        self.turns.push(self.last.clone());
        let mut attached = Vec::new();
        for url in strings(&payload["images"]) {
            attached.extend(images::from_data_url(self.blobs, url)?);
        }
        for file in strings(&payload["local_images"]) {
            attached.extend(images::from_file(self.blobs, Path::new(file))?);
        }
        let content = Content {
            text: payload["message"].as_str().unwrap_or_default().to_owned(),
            images: attached,
        };
        Ok(normalize::user(content).into_iter().collect())
    }

    fn response_item(&self, item: &Value, paginated: bool) -> Result<Vec<Body>> {
        let call = |name: &str, input: Value| -> Result<Vec<Body>> {
            let id = if item["call_id"].is_string() {
                &item["call_id"]
            } else {
                &item["id"]
            };
            Ok(vec![Body::ToolCall(ToolCall {
                call_id: normalize::call_id(id)?,
                name: name.to_owned(),
                input,
            })])
        };
        let name = item["name"].as_str().unwrap_or("unknown");
        let bodies = match item["type"].as_str().unwrap_or_default() {
            "message" if !paginated && item["role"] == "assistant" => {
                let mut bodies = Vec::new();
                for text in strings_at(&item["content"], "text") {
                    bodies.extend(normalize::assistant(text));
                }
                bodies
            }
            "function_call" => call(name, normalize::arguments(&item["arguments"]))?,
            "custom_tool_call" => call(name, json!({"input": item["input"]}))?,
            "local_shell_call" => {
                call("local_shell", normalize::tool_input(item["action"].clone()))?
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                let output = self.output(&item["output"])?;
                vec![Body::ToolResult(ToolResult {
                    call_id: normalize::call_id(&item["call_id"])?,
                    text: output.content.text,
                    images: output.content.images,
                    is_error: output.is_error,
                })]
            }
            "reasoning" => {
                let mut texts: Vec<&str> = Vec::new();
                for key in ["summary", "content"] {
                    texts.extend(strings_at(&item[key], "text"));
                }
                let thinking = Thinking {
                    text: texts.join("\n"),
                    signature: json::string(&item["encrypted_content"]),
                };
                if thinking.text.is_empty() && thinking.signature.is_none() {
                    Vec::new()
                } else {
                    vec![Body::Thinking(thinking)]
                }
            }
            "web_search_call" => {
                vec![Body::Meta(
                    json!({"type": "web_search", "action": item["action"]}),
                )]
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn output(&self, output: &Value) -> Result<Output> {
        let output = match output {
            Value::String(text) => Output {
                content: Content {
                    text: text.clone(),
                    images: Vec::new(),
                },
                is_error: false,
            },
            Value::Array(items) => {
                let mut attached = Vec::new();
                for url in strings_at(output, "image_url") {
                    attached.extend(images::from_data_url(self.blobs, url)?);
                }
                Output {
                    content: Content {
                        text: json::texts(items, &["input_text", "output_text", "text"]),
                        images: attached,
                    },
                    is_error: false,
                }
            }
            // Older versions stored the output with a success flag
            Value::Object(fields) => Output {
                content: self.output(&output["content"])?.content,
                is_error: fields.get("success") == Some(&Value::Bool(false)),
            },
            _ => Output {
                content: Content {
                    text: String::new(),
                    images: Vec::new(),
                },
                is_error: false,
            },
        };
        Ok(output)
    }

    fn completed_item(&mut self, item: &Value) -> Result<Vec<Body>> {
        let blocks = item["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let bodies = match item["type"].as_str().unwrap_or_default() {
            "UserMessage" => {
                self.turns.push(self.last.clone());
                let mut attached = Vec::new();
                for block in blocks {
                    match block["type"].as_str().unwrap_or_default() {
                        "image" => {
                            if let Some(url) = block["image_url"].as_str() {
                                attached.extend(images::from_data_url(self.blobs, url)?);
                            }
                        }
                        "local_image" => {
                            if let Some(path) = block["path"].as_str() {
                                attached.extend(images::from_file(self.blobs, Path::new(path))?);
                            }
                        }
                        _ => {}
                    }
                }
                let content = Content {
                    text: json::texts(blocks, &["text", "Text"]),
                    images: attached,
                };
                normalize::user(content).into_iter().collect()
            }
            "AgentMessage" => {
                let mut bodies = Vec::new();
                for block in blocks {
                    bodies.extend(block["text"].as_str().and_then(normalize::assistant));
                }
                bodies
            }
            _ => Vec::new(),
        };
        Ok(bodies)
    }

    fn rollback(&mut self, turns: u64) -> Line {
        for _ in 0..turns {
            match self.turns.pop() {
                Some(before) => self.last = before,
                // An incremental read never saw the turns it would have to undo
                None if self.continuing => return Line::Restart,
                None => {
                    self.last = None;
                    break;
                }
            }
        }
        Line::Skip
    }
}

impl Parser for RolloutParser<'_> {
    fn line(&mut self, offset: u64, record: Value) -> Result<Line> {
        let payload = &record["payload"];
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("session_meta"), _) => {
                self.info.cwd = json::string(&payload["cwd"]);
                self.info.created = payload["timestamp"]
                    .as_str()
                    .or(record["timestamp"].as_str())
                    .and_then(time::normalize);
                return Ok(Line::Skip);
            }
            (Some("event_msg"), Some("thread_name_updated")) => {
                self.info.title = json::string(&payload["thread_name"]).or(self.info.title.take());
                return Ok(Line::Skip);
            }
            (Some("event_msg"), Some("thread_rolled_back")) => {
                return Ok(self.rollback(payload["num_turns"].as_u64().unwrap_or(1)));
            }
            _ => {}
        }
        let bodies = self.bodies(&record)?;
        if bodies.is_empty() {
            return Ok(Line::Skip);
        }
        let fallback = self.info.created.clone().unwrap_or_else(time::now);
        let timestamp = json::timestamp(&record["timestamp"], &fallback);
        // Rollout records have no ids, so their byte offsets stand in
        let id = offset.to_string();
        let parent = self.last.replace(id.clone());
        let native_entry_id = json::string(&payload["id"]);
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

    fn finish(self) -> Info {
        self.info
    }
}

// The string elements of a JSON array
fn strings(value: &Value) -> Vec<&str> {
    let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
    items.iter().filter_map(Value::as_str).collect()
}

// The string field `key` of each element of a JSON array
fn strings_at<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
    let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
    items.iter().filter_map(|item| item[key].as_str()).collect()
}
