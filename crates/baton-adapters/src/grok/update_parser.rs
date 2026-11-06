use std::collections::{HashMap, HashSet};

use anyhow::Result;
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, journal, time,
};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::{images, json};

// What baton writes in front of a summary, in a user chunk marked as its compaction
pub(super) const SUMMARY_PREFIX: &str = "[Imported conversation summary]\n";

// Grok streams a message as consecutive chunks, so lines only become bodies once the next line shows the message ended
pub(super) fn drafts(bytes: &[u8], fallback: &str, blobs: &Blobs) -> Result<Vec<Draft>> {
    let records = rewound(bytes);
    // A call recorded before its arguments were known gets them from its first update that has them
    let mut late_inputs: HashMap<&str, &Value> = HashMap::new();
    for record in &records {
        let update = update(record);
        if update["sessionUpdate"] == "tool_call_update"
            && !update["rawInput"].is_null()
            && let Some(id) = update["toolCallId"].as_str()
        {
            late_inputs.entry(id).or_insert(&update["rawInput"]);
        }
    }
    let mut reader = Reader {
        blobs,
        drafts: Vec::new(),
        run: None,
        settled: HashSet::new(),
    };
    for record in &records {
        let timestamp = record["params"]["_meta"]["agentTimestampMs"]
            .as_i64()
            .or_else(|| record["timestamp"].as_i64().map(|seconds| seconds * 1000))
            .and_then(time::from_unix_millis)
            .unwrap_or_else(|| fallback.to_owned());
        let update = update(record);
        let meta = &update["_meta"];
        match (
            acp(record),
            update["sessionUpdate"].as_str().unwrap_or_default(),
        ) {
            (true, "user_message_chunk") if meta["hostTurn"] != true => {
                let text = update["content"]["text"].as_str().unwrap_or_default();
                if meta["baton"] == "compaction" {
                    reader.flush();
                    let summary = text.strip_prefix(SUMMARY_PREFIX).unwrap_or(text).to_owned();
                    reader.push(timestamp, Body::Compaction(Compaction { summary }));
                    continue;
                }
                // A message sent in the middle of a turn is a message of its own
                let key = if meta["interjection"] == true {
                    json!({"interjection": timestamp})
                } else {
                    json!([meta["promptIndex"], update["messageId"]])
                };
                reader.extend(Kind::User, key, timestamp);
                let image = reader.image(&update["content"])?;
                let run = reader.run.as_mut().expect("a run was just extended");
                if update["content"]["type"] == "text" {
                    run.texts.push(text.to_owned());
                }
                run.images.extend(image);
            }
            (true, kind @ ("agent_message_chunk" | "agent_thought_chunk")) => {
                let kind = if kind == "agent_message_chunk" {
                    Kind::Assistant
                } else {
                    Kind::Thinking
                };
                reader.extend(kind, update["messageId"].clone(), timestamp);
                let text = update["content"]["text"].as_str().unwrap_or_default();
                let run = reader.run.as_mut().expect("a run was just extended");
                run.texts.push(text.to_owned());
            }
            (true, "tool_call") => {
                reader.flush();
                let id = update["toolCallId"].as_str().unwrap_or_default();
                let input = match &update["rawInput"] {
                    Value::Null => late_inputs.get(id).copied().cloned().unwrap_or(Value::Null),
                    input => input.clone(),
                };
                let name = meta["x.ai/tool"]["name"]
                    .as_str()
                    .or(update["title"].as_str())
                    .unwrap_or("unknown");
                let call = Body::ToolCall(ToolCall {
                    call_id: normalize::call_id(&update["toolCallId"]),
                    name: name.to_owned(),
                    input: normalize::tool_input(input),
                });
                reader.push(timestamp, call);
            }
            (true, "tool_call_update")
                if matches!(update["status"].as_str(), Some("completed" | "failed")) =>
            {
                reader.flush();
                let id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
                // Grok only takes a call's first final update
                if reader.settled.insert(id) {
                    let result = reader.result(update)?;
                    reader.push(timestamp, result);
                }
            }
            (false, "compaction_checkpoint") => {
                // Grok keeps its summary outside the log, so the visible history stays in force
                reader.flush();
                let checkpoint = &update["checkpoint_file"];
                reader.push(
                    timestamp,
                    Body::Meta(json!({"type": "compaction", "checkpoint": checkpoint})),
                );
            }
            (false, kind @ ("model_changed" | "model_auto_switched")) => {
                reader.flush();
                let model = if kind == "model_changed" {
                    &update["model_id"]
                } else {
                    &update["new_model_id"]
                };
                reader.push(
                    timestamp,
                    Body::Meta(json!({"type": "model", "model": model})),
                );
            }
            _ => reader.flush(),
        }
    }
    reader.flush();
    Ok(reader.drafts)
}

// The lines a `/rewind` left in force: a rewind marker cuts the log back to the start of the prompt it targets
fn rewound(bytes: &[u8]) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::new();
    let mut prompt_starts: Vec<usize> = Vec::new();
    // Prompts count unmarked until Grok marks the first one with its index, and only marked ones after that
    let mut marked = false;
    let mut prompt: Option<Value> = None;
    for line in journal::lines(bytes) {
        let Ok(record) = serde_json::from_slice::<Value>(line.bytes) else {
            continue;
        };
        let update = update(&record);
        if !acp(&record) && update["sessionUpdate"] == "rewind_marker" {
            let target = update["target_prompt_index"].as_u64().unwrap_or(u64::MAX) as usize;
            if let Some(&start) = prompt_starts.get(target) {
                kept.truncate(start);
            }
            prompt_starts.truncate(target);
            prompt = None;
            continue;
        }
        if acp(&record)
            && update["sessionUpdate"] == "user_message_chunk"
            && update["_meta"]["hostTurn"] != true
        {
            let index = update["_meta"]["promptIndex"].clone();
            marked |= !index.is_null();
            if prompt.as_ref() != Some(&index) {
                if !marked || !index.is_null() {
                    prompt_starts.push(kept.len());
                }
                prompt = Some(index);
            }
        } else {
            prompt = None;
        }
        kept.push(record);
    }
    kept
}

// Grok logs ACP updates and its own extension updates side by side; a line without a method is a bare ACP notification
fn acp(record: &Value) -> bool {
    record["method"]
        .as_str()
        .is_none_or(|method| method == "session/update")
}

fn update(record: &Value) -> &Value {
    if record["params"].is_object() {
        &record["params"]["update"]
    } else {
        &record["update"]
    }
}

#[derive(PartialEq)]
enum Kind {
    User,
    Assistant,
    Thinking,
}

// Consecutive chunks of one message
struct Run {
    kind: Kind,
    key: Value,
    timestamp: String,
    texts: Vec<String>,
    images: Vec<Image>,
}

struct Reader<'a> {
    blobs: &'a Blobs,
    drafts: Vec<Draft>,
    run: Option<Run>,
    settled: HashSet<String>,
}

impl Reader<'_> {
    // Continues the current run when the chunk belongs to it, else starts a new one
    fn extend(&mut self, kind: Kind, key: Value, timestamp: String) {
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.kind == kind && run.key == key)
        {
            return;
        }
        self.flush();
        self.run = Some(Run {
            kind,
            key,
            timestamp,
            texts: Vec::new(),
            images: Vec::new(),
        });
    }

    fn flush(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        let body = match run.kind {
            // Grok joins the text chunks of a prompt, while a reply streams as fragments of one text
            Kind::User => normalize::user(Content {
                text: run.texts.join("\n"),
                images: run.images,
            }),
            Kind::Assistant => normalize::assistant(&run.texts.concat()),
            Kind::Thinking => {
                let text = run.texts.concat();
                (!text.is_empty()).then_some(Body::Thinking(Thinking {
                    text,
                    signature: None,
                }))
            }
        };
        if let Some(body) = body {
            self.push(run.timestamp, body);
        }
    }

    fn push(&mut self, timestamp: String, body: Body) {
        self.drafts.push(Draft {
            timestamp,
            native_entry_id: None,
            body,
        });
    }

    fn result(&self, update: &Value) -> Result<Body> {
        let blocks = update["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut texts = Vec::new();
        let mut attached = Vec::new();
        for block in blocks {
            let content = &block["content"];
            match content["type"].as_str().unwrap_or_default() {
                "text" => texts.push(content["text"].as_str().unwrap_or_default()),
                "image" => attached.extend(self.image(content)?),
                _ => {}
            }
        }
        // A result without content blocks still has the tool's raw output
        let text = if texts.is_empty() && attached.is_empty() && !update["rawOutput"].is_null() {
            update["rawOutput"].to_string()
        } else {
            texts.join("\n")
        };
        Ok(Body::ToolResult(ToolResult {
            call_id: normalize::call_id(&update["toolCallId"]),
            text,
            images: attached,
            is_error: update["status"] == "failed",
        }))
    }

    fn image(&self, content: &Value) -> Result<Option<Image>> {
        if content["type"] != "image" {
            return Ok(None);
        }
        if let (Some(data), Some(mime)) = (content["data"].as_str(), content["mimeType"].as_str())
            && let Some(bytes) = images::decode(data)
        {
            return images::store(self.blobs, mime, &bytes).map(Some);
        }
        images::from_data_url(self.blobs, content["uri"].as_str().unwrap_or_default())
    }
}

// The title Grok shows: its generated or manual title, else the session summary
pub(super) fn title(summary: &Value) -> Option<String> {
    let generated = summary["generated_title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    generated
        .map(String::from)
        .or_else(|| json::string(&summary["session_summary"]))
}
