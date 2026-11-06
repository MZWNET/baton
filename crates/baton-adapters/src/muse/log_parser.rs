use anyhow::Result;
use baton_core::journal::{Line, Node, Parser};
use baton_core::{Body, Compaction, Draft, Info, Thinking, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use crate::muse::markers;
use crate::support::{json, normalize};

// Muse marks the results of failed tools only in their text
const FAILED: &str = "tool failed: ";

// Reads a Muse session log, one stream of envelopes whose order is the conversation
pub(super) struct LogParser<'a> {
    session: &'a str,
    last: Option<String>,
    model: Option<String>,
    info: Info,
}

impl<'a> LogParser<'a> {
    pub(super) fn new(session: &'a str, leaf: Option<&str>) -> LogParser<'a> {
        LogParser {
            session,
            last: leaf.map(String::from),
            model: None,
            info: Info::default(),
        }
    }

    // Records mirrored from other streams belong to those streams
    fn bodies(&mut self, envelope: &Value) -> Vec<Body> {
        if envelope["stream"]["id"] != self.session {
            return Vec::new();
        }
        if self.info.created.is_none() {
            self.info.created = envelope["recorded_at"]
                .as_i64()
                .and_then(|micros| time::from_unix_millis(micros / 1000));
        }
        let payload = &envelope["payload"];
        match envelope["payload_type"].as_str().unwrap_or_default() {
            "runtime.session" if payload["kind"] == "run" => self.run_event(&payload["event"]),
            "runtime.session.metadata" | "runtime.session.route_facts" => {
                let record = &payload["record"];
                if self.info.cwd.is_none() {
                    self.info.cwd = json::string(&record["workspace_root"])
                        .or_else(|| json::string(&payload["workspace_root"]))
                        .or_else(|| json::string(&record["cwd"]));
                }
                Vec::new()
            }
            // Muse names sessions with random handles until the user names them
            "session.name.changed" if payload["source"] == "user" => {
                self.info.title = json::string(&payload["new_name"]).or(self.info.title.take());
                Vec::new()
            }
            "run.model.configured" => self.model(&payload["record"]["model_id"]),
            _ => Vec::new(),
        }
    }

    fn run_event(&mut self, event: &Value) -> Vec<Body> {
        match event["kind"].as_str().unwrap_or_default() {
            "started" => prompt(event["prompt"].as_str().unwrap_or_default()),
            "assistant_message_committed" => event["text"]
                .as_str()
                .and_then(normalize::assistant)
                .into_iter()
                .collect(),
            "assistant_tool_calls_committed" => {
                let calls = event["tool_calls"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut bodies = Vec::with_capacity(calls.len());
                for call in calls {
                    let id = if call["call_id"].is_string() {
                        &call["call_id"]
                    } else {
                        &call["id"]
                    };
                    bodies.push(Body::ToolCall(ToolCall {
                        call_id: normalize::call_id(id),
                        name: call["name"].as_str().unwrap_or("unknown").to_owned(),
                        input: normalize::arguments(&call["args"]),
                    }));
                }
                bodies
            }
            "tool_result_batch_committed" | "tool_results_committed" => {
                let results = event["results"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                results.iter().map(result).collect()
            }
            "tool_result_committed" | "tool_result" => vec![result(event)],
            "reasoning_summary_committed" | "reasoning_committed" => {
                let thinking = Thinking {
                    text: event["text"].as_str().unwrap_or_default().to_owned(),
                    signature: json::string(&event["encrypted_content"]),
                };
                if thinking.text.is_empty() && thinking.signature.is_none() {
                    Vec::new()
                } else {
                    vec![Body::Thinking(thinking)]
                }
            }
            "model_completed" => self.model(&event["model"]),
            // Muse keeps no summary of its own compactions in the log, so the history stays in force
            "context_compaction_installed" => vec![Body::Meta(json!({"type": "compaction"}))],
            _ => Vec::new(),
        }
    }

    fn model(&mut self, model: &Value) -> Vec<Body> {
        match json::string(model) {
            Some(model) if self.model.as_ref() != Some(&model) => {
                self.model = Some(model.clone());
                vec![Body::Meta(json!({"type": "model", "model": model}))]
            }
            _ => Vec::new(),
        }
    }
}

// A prompt is what the user typed, unless baton wrote a marker turn
fn prompt(text: &str) -> Vec<Body> {
    if text == markers::CONTINUATION {
        return Vec::new();
    }
    if let Some(summary) = markers::unwrap_summary(text) {
        return vec![Body::Compaction(Compaction {
            summary: summary.to_owned(),
        })];
    }
    normalize::user(markers::decode(text)).into_iter().collect()
}

fn result(entry: &Value) -> Body {
    let text = ["text", "output", "result"]
        .iter()
        .find_map(|key| entry[key].as_str())
        .unwrap_or_default();
    let id = if entry["tool_call_id"].is_string() {
        &entry["tool_call_id"]
    } else {
        &entry["call_id"]
    };
    let (content, is_error) = markers::decode_result(text);
    Body::ToolResult(ToolResult {
        call_id: normalize::call_id(id),
        is_error: is_error || text.starts_with(FAILED),
        text: content.text,
        images: content.images,
    })
}

impl Parser for LogParser<'_> {
    fn line(&mut self, offset: u64, line: Value) -> Result<Line> {
        // A retained frame carries envelopes serialized as strings; markers stand in for omitted live records
        let envelopes: Vec<Value> = if line.get("retained_marker").is_some() {
            Vec::new()
        } else if line.get("retained_frame").is_some() {
            let children = line["children"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            children
                .iter()
                .filter_map(|child| serde_json::from_str(child["record_json"].as_str()?).ok())
                .collect()
        } else {
            vec![line]
        };
        let mut drafts = Vec::new();
        for envelope in &envelopes {
            let fallback = self.info.created.clone().unwrap_or_else(time::now);
            let timestamp = envelope["recorded_at"]
                .as_i64()
                .and_then(|micros| time::from_unix_millis(micros / 1000))
                .unwrap_or(fallback);
            for body in self.bodies(envelope) {
                drafts.push(Draft {
                    timestamp: timestamp.clone(),
                    native_entry_id: json::string(&envelope["id"]),
                    body,
                });
            }
        }
        if drafts.is_empty() {
            return Ok(Line::Skip);
        }
        // Lines have no ids of their own, so their byte offsets stand in
        let id = offset.to_string();
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
