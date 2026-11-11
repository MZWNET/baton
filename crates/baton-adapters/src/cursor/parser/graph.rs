// The agent graph Cursor keeps for a chat, alike in the CLI's stores and the editor's database: protobuf state and turns, with the model's history as JSON messages, all in blobs named by their SHA-256

use std::collections::{HashMap, VecDeque};
use std::str;

use anyhow::{Context, Result};
use baton_core::{Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use crate::support::normalize::{self, Content};
use crate::support::protobuf::Message;
use crate::support::{images, json, uri};

// Loads a blob by its lowercase hex id
pub(in crate::cursor) type Load<'a> = dyn Fn(&str) -> Result<Vec<u8>> + 'a;

pub(in crate::cursor) struct Graph {
    pub(in crate::cursor) drafts: Vec<Draft>,
    pub(in crate::cursor) cwd: Option<String>,
    pub(in crate::cursor) created: Option<String>,
}

// The chat whose latest `ConversationStateStructure` is `root`; `created` is the creation time in milliseconds its store records, if any
pub(in crate::cursor) fn read(
    root: &[u8],
    created: Option<i64>,
    load: &Load,
    blobs: &Blobs,
) -> Result<Graph> {
    let state = Message::parse(root)?;
    let created = created.or_else(|| state.varint(26).map(|millis| millis as i64));
    let mut reader = Reader {
        load,
        blobs,
        times: times(&state, load)?,
        turn: None,
        clock: created.unwrap_or_else(|| time::unix_millis(&time::now())),
        model: None,
        drafts: Vec::new(),
    };
    // The model's history starts after the last compaction; the archives keep what each one replaced
    let mut archives = state.repeated(13).collect::<Vec<_>>();
    if let Some(legacy) = state.bytes(11)
        && !archives.contains(&legacy)
    {
        archives.insert(0, legacy);
    }
    for id in &archives {
        let hex = base16ct::lower::encode_string(id);
        let bytes = load(&hex)?;
        let archive = Message::parse(&bytes)?;
        for message in archive.repeated(1) {
            reader.message(message, true)?;
        }
        let summary = match (
            archive.string(2).filter(|summary| !summary.is_empty()),
            archive.bytes(4),
        ) {
            (Some(summary), _) => summary.to_owned(),
            (None, Some(message)) => text(&reader.json(message)?.1),
            (None, None) => String::new(),
        };
        reader.push(&hex, None, Body::Compaction(Compaction { summary }));
    }
    for message in state.repeated(1) {
        reader.message(message, !archives.is_empty())?;
    }
    let workspace = state
        .repeated(9)
        .next()
        .and_then(|link| str::from_utf8(link).ok());
    Ok(Graph {
        drafts: reader.drafts,
        cwd: workspace.and_then(uri::path),
        created: created.and_then(time::from_unix_millis),
    })
}

// When the turns of a chat say its messages and tool calls happened, in milliseconds
#[derive(Default)]
struct Times {
    // By the request id a user message shares with its turn
    users: HashMap<String, i64>,
    // The start of each assistant reply of a turn, in order
    replies: HashMap<String, VecDeque<Option<i64>>>,
    // The start and the end of each tool call
    calls: HashMap<String, (Option<i64>, Option<i64>)>,
}

fn times(state: &Message, load: &Load) -> Result<Times> {
    let mut times = Times::default();
    for turn in state.repeated(8) {
        let bytes = load(&base16ct::lower::encode_string(turn))?;
        // Shell turns, the user's own `!` commands, never reach the model
        let Some(agent) = Message::parse(&bytes)?.message(1) else {
            continue;
        };
        let request = agent.string(3).unwrap_or_default().to_owned();
        if let Some(user) = agent.bytes(1) {
            let user = load(&base16ct::lower::encode_string(user))?;
            if let Some(started) = Message::parse(&user)?.varint(25) {
                times.users.insert(request.clone(), started as i64);
            }
        }
        for step in agent.repeated(2) {
            let step = load(&base16ct::lower::encode_string(step))?;
            let step = Message::parse(&step)?;
            if let Some(reply) = step.message(1) {
                let started = reply.varint(2).map(|millis| millis as i64);
                times
                    .replies
                    .entry(request.clone())
                    .or_default()
                    .push_back(started);
            }
            if let Some(call) = step.message(2)
                && let Some(id) = call.string(57)
            {
                let millis = |field| call.varint(field).map(|millis| millis as i64);
                times.calls.insert(id.to_owned(), (millis(59), millis(60)));
            }
        }
    }
    Ok(times)
}

struct Reader<'a> {
    load: &'a Load<'a>,
    blobs: &'a Blobs,
    times: Times,
    // The request id of the last user message, whose turn holds the replies that follow
    turn: Option<String>,
    // The time of the last event in milliseconds, which events without a time of their own take
    clock: i64,
    model: Option<String>,
    drafts: Vec<Draft>,
}

impl Reader<'_> {
    fn json(&self, id: &[u8]) -> Result<(String, Value)> {
        let hex = base16ct::lower::encode_string(id);
        let message = serde_json::from_slice(&(self.load)(&hex)?)
            .with_context(|| format!("reading Cursor message {hex}"))?;
        Ok((hex, message))
    }

    // `archived` says whether summary messages are already covered by archives
    fn message(&mut self, id: &[u8], archived: bool) -> Result<()> {
        let (id, message) = self.json(id)?;
        let options = &message["providerOptions"];
        let parts = message["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        match message["role"].as_str() {
            Some("user") if options["baton"]["synthetic"] == true => {}
            Some("user") if options["cursor"]["isSummary"] == true => {
                if !archived {
                    self.push(
                        &id,
                        None,
                        Body::Compaction(Compaction {
                            summary: text(&message),
                        }),
                    );
                }
            }
            Some("user") => self.user(&id, &message, parts)?,
            Some("assistant") => {
                if let Some(text) = message["content"].as_str() {
                    self.reply(&id, text);
                }
                for part in parts {
                    self.assistant_part(&id, part)?;
                }
            }
            Some("tool") => {
                let failed = options["cursor"]["highLevelToolCallResult"]["isError"] == true;
                for part in parts.iter().filter(|part| part["type"] == "tool-result") {
                    self.result(&id, part, failed)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn user(&mut self, id: &str, message: &Value, parts: &[Value]) -> Result<()> {
        let text = text(message);
        // Cursor hands the model its environment, rules and skills as a user message of their own
        if text.trim_start().starts_with("<user_info>") {
            return Ok(());
        }
        let mut images = Vec::new();
        for part in parts {
            let mime = part["mimeType"].as_str().unwrap_or("image/png");
            let data = match part["type"].as_str() {
                Some("image") => &part["image"],
                Some("file") if mime.starts_with("image/") => &part["data"],
                _ => continue,
            };
            images.extend(image(self.blobs, data, mime)?);
        }
        let request = message["providerOptions"]["cursor"]["requestId"].as_str();
        let millis = request.and_then(|request| self.times.users.get(request).copied());
        self.turn = request.map(String::from);
        if let Some(body) = normalize::user(Content {
            text: query(&text),
            images,
        }) {
            self.push(id, millis, body);
        }
        Ok(())
    }

    fn assistant_part(&mut self, id: &str, part: &Value) -> Result<()> {
        if let Some(model) = json::string(&part["providerOptions"]["cursor"]["modelName"])
            && self.model.as_ref() != Some(&model)
        {
            self.push(
                id,
                None,
                Body::Meta(json!({"type": "model", "model": model})),
            );
            self.model = Some(model);
        }
        match part["type"].as_str() {
            Some("text") => self.reply(id, part["text"].as_str().unwrap_or_default()),
            Some("reasoning") => self.push(
                id,
                None,
                Body::Thinking(Thinking {
                    text: part["text"].as_str().unwrap_or_default().to_owned(),
                    signature: json::string(&part["signature"]),
                }),
            ),
            Some("redacted-reasoning") => self.push(
                id,
                None,
                Body::Thinking(Thinking {
                    text: String::new(),
                    signature: json::string(&part["data"]),
                }),
            ),
            Some("tool-call") => {
                let started = self.call_times(&part["toolCallId"]).0;
                let call = ToolCall {
                    call_id: normalize::call_id(&part["toolCallId"])?,
                    name: part["toolName"].as_str().unwrap_or("unknown").to_owned(),
                    input: normalize::tool_input(part["args"].clone()),
                };
                self.push(id, started, Body::ToolCall(call));
            }
            _ => {}
        }
        Ok(())
    }

    fn reply(&mut self, id: &str, text: &str) {
        let started = self
            .turn
            .as_ref()
            .and_then(|turn| self.times.replies.get_mut(turn)?.pop_front())
            .flatten();
        if let Some(body) = normalize::assistant(text) {
            self.push(id, started, body);
        }
    }

    fn result(&mut self, id: &str, part: &Value, failed: bool) -> Result<()> {
        let (text, images) = match part["experimental_content"].as_array() {
            Some(content) => {
                let mut images = Vec::new();
                for item in content.iter().filter(|item| item["type"] == "image") {
                    images.extend(image(
                        self.blobs,
                        &item["data"],
                        item["mimeType"].as_str().unwrap_or("image/png"),
                    )?);
                }
                (json::texts(content, &["text"]), images)
            }
            None => (json::to_text(&part["result"]), Vec::new()),
        };
        let (started, ended) = self.call_times(&part["toolCallId"]);
        let result = ToolResult {
            call_id: normalize::call_id(&part["toolCallId"])?,
            text,
            images,
            is_error: failed || part["isError"] == true,
        };
        self.push(id, ended.or(started), Body::ToolResult(result));
        Ok(())
    }

    fn call_times(&self, id: &Value) -> (Option<i64>, Option<i64>) {
        id.as_str()
            .and_then(|id| self.times.calls.get(id).copied())
            .unwrap_or_default()
    }

    // Times never go back, so an event the turns give no time takes the one before it
    fn push(&mut self, id: &str, millis: Option<i64>, body: Body) {
        if let Some(millis) = millis {
            self.clock = self.clock.max(millis);
        }
        self.drafts.push(Draft {
            timestamp: time::from_unix_millis(self.clock).unwrap_or_else(time::now),
            native_entry_id: Some(id.to_owned()),
            body,
        });
    }
}

// The text of a message, whose content is a string or a list of parts
fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        content => json::texts(
            content.as_array().map(Vec::as_slice).unwrap_or_default(),
            &["text"],
        ),
    }
}

// Cursor wraps what the user typed in a query tag, after a timestamp and before any context it attaches
fn query(text: &str) -> String {
    let Some((_, inner)) = text.split_once("<user_query>") else {
        return text.to_owned();
    };
    let inner = inner
        .rsplit_once("</user_query>")
        .map_or(inner, |(inner, _)| inner);
    let inner = inner.strip_prefix('\n').unwrap_or(inner);
    inner.strip_suffix('\n').unwrap_or(inner).to_owned()
}

// Image data as a data URL, as base64 or as the hex Cursor turns byte arrays into; links to remote images are left out
fn image(blobs: &Blobs, data: &Value, mime: &str) -> Result<Option<Image>> {
    let bytes = match data {
        Value::String(url) if url.starts_with("data:") => return images::from_data_url(blobs, url),
        Value::String(url) if url.starts_with("http://") || url.starts_with("https://") => None,
        Value::String(base64) => images::decode(base64),
        _ if data["__type"] == "Uint8Array" => data["hex"]
            .as_str()
            .and_then(|hex| base16ct::mixed::decode_vec(hex).ok()),
        _ => None,
    };
    bytes
        .map(|bytes| images::store(blobs, mime, &bytes))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_come_out_of_their_tags_exactly() {
        let wrapped = "<timestamp>Monday</timestamp>\n<user_query>\n\nsay </user_query> twice\n\n</user_query>\n<context/>";
        assert_eq!(query(wrapped), "\nsay </user_query> twice\n");
        assert_eq!(query("plain"), "plain");
    }
}
