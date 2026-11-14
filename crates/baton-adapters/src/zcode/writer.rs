use anyhow::Result;
use baton_core::replay::Models;
use baton_core::{Blobs, Body, Event, Export, Image, ToolResult, time};
use serde_json::{Value, json};

use crate::support::model::provider_and_id;
use crate::support::tool_order::{self, Placement, ToolOrder};
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images};

pub(super) struct Session {
    pub(super) id: String,
    pub(super) project_id: String,
    pub(super) slug: String,
    pub(super) directory: String,
    pub(super) title: String,
    pub(super) title_source: &'static str,
    pub(super) trace_id: String,
    pub(super) created: i64,
    pub(super) updated: i64,
    pub(super) messages: Vec<Message>,
    pub(super) model_selection: Option<Value>,
}

pub(super) struct Message {
    pub(super) id: String,
    pub(super) created: i64,
    pub(super) updated: i64,
    pub(super) data: Value,
    pub(super) parts: Vec<Part>,
}

pub(super) struct Part {
    pub(super) id: String,
    pub(super) created: i64,
    pub(super) data: Value,
}

pub(super) fn session(export: &Export, blobs: &Blobs) -> Result<Session> {
    let id = format!("sess_{}", identifiers::session_v4(export.seed));
    let directory = export.cwd.to_string_lossy().into_owned();
    let project = slugify(&directory);
    let (title, title_source) = match export.title {
        Some(title) => (title.to_owned(), "custom"),
        None => {
            let first = export.events.iter().find_map(|event| match &event.body {
                Body::User(user) => Some(user.text.as_str()),
                _ => None,
            });
            (title_from_input(first.unwrap_or_default()), "first_input")
        }
    };
    let model_selection = export.models.last().and_then(|model| {
        let (provider, id) = model.split();
        // ZCode resumes on this selection rather than the models of the messages
        provider.map(|provider| json!({"modelSelection": {"providerId": provider, "modelId": id}}))
    });
    let mut writer = Writer {
        blobs,
        models: export.models,
        session: &id,
        cwd: &directory,
        messages: Vec::new(),
        calls: ToolOrder::default(),
        last_user: None,
    };
    writer.push_events(export.events)?;
    // The newest session is the one zcode --continue picks
    let updated = time::unix_millis(&time::now())
        .max(writer.messages.last().map_or(0, |message| message.created));
    let messages = writer.messages;
    Ok(Session {
        slug: slugify(&id),
        id,
        project_id: format!("proj_{}", &project[..project.len().min(80)]),
        directory,
        title,
        title_source,
        trace_id: identifiers::uuid_v4(),
        created: time::unix_millis(export.created),
        updated,
        messages,
        model_selection,
    })
}

struct Writer<'a> {
    blobs: &'a Blobs,
    models: &'a Models<'a>,
    session: &'a str,
    cwd: &'a str,
    messages: Vec<Message>,
    calls: ToolOrder<'a>,
    last_user: Option<String>,
}

impl<'a> Writer<'a> {
    fn push_events(&mut self, events: &[&'a Event]) -> Result<()> {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    self.calls.clear();
                    let millis = time::unix_millis(&event.timestamp);
                    let id = sortable_id("msg", millis);
                    let mut parts = Vec::new();
                    if !user.text.is_empty() {
                        parts.push(part(millis, json!({"type": "text", "text": user.text, "time": {"start": millis, "end": millis}})));
                    }
                    for image in &user.images {
                        parts.push(part(millis, self.file(image)?));
                    }
                    let (provider, model) = provider_and_id(self.models.at(event));
                    let data = json!({
                        "role": "user", "time": {"created": millis}, "agent": "zcode-agent",
                        "modelSelection": {"providerId": provider, "modelId": model},
                        "model": {"providerID": provider, "modelID": model},
                        "semantics": {"origin": "real_user", "kind": "user_prompt", "uiVisibility": "visible", "providerVisibility": "visible", "transcriptVisibility": "visible"},
                    });
                    self.messages.push(Message {
                        id: id.clone(),
                        created: millis,
                        updated: millis,
                        data,
                        parts,
                    });
                    self.last_user = Some(id);
                }
                Unit::Assistant(events) => {
                    let millis = time::unix_millis(&events[0].timestamp);
                    let id = sortable_id("msg", millis);
                    let mut parts = Vec::new();
                    for &event in &events {
                        // ZCode orders assistant blocks by this index when every block has one
                        let index = parts.len();
                        let data = match &event.body {
                            Body::Assistant(assistant) => {
                                json!({"type": "text", "text": assistant.text, "blockIndex": index, "time": {"start": millis, "end": millis}})
                            }
                            Body::ToolCall(call) => {
                                self.calls.insert(call, self.messages.len(), index);
                                json!({"type": "tool", "callID": call.call_id, "tool": call.name, "blockIndex": index, "metadata": {}, "state": {"status": "pending", "input": call.input, "raw": ""}})
                            }
                            _ => continue,
                        };
                        parts.push(part(millis, data));
                    }
                    self.push_assistant(id, millis, parts, events[0]);
                    self.calls.assistant(&events);
                }
                Unit::ToolResult(event, result) => self.store_result(event, result)?,
                Unit::Compaction(event, summary) => {
                    self.calls.clear();
                    let millis = time::unix_millis(&event.timestamp);
                    let id = sortable_id("msg", millis);
                    let data = json!({
                        "role": "user", "time": {"created": millis}, "agent": "zcode-agent", "model": {},
                        "summary": {"title": "Compact summary", "body": summary, "diffs": []},
                        "semantics": {"origin": "agent_runtime", "kind": "compact_summary", "uiVisibility": "hidden", "providerVisibility": "visible", "transcriptVisibility": "hidden"},
                    });
                    let parts = vec![
                        part(
                            millis,
                            json!({"type": "text", "text": summary, "synthetic": true, "time": {"start": millis, "end": millis}}),
                        ),
                        part(millis, json!({"type": "compaction", "auto": true})),
                    ];
                    self.messages.push(Message {
                        id: id.clone(),
                        created: millis,
                        updated: millis,
                        data,
                        parts,
                    });
                    self.last_user = Some(id);
                }
            }
        }
        Ok(())
    }

    fn push_assistant(&mut self, id: String, millis: i64, parts: Vec<Part>, event: &Event) {
        let (provider, model) = provider_and_id(self.models.at(event));
        let parent = self.last_user.as_deref().unwrap_or(&id);
        let finish = if parts.iter().any(|part| part.data["type"] == "tool") {
            "tool-calls"
        } else {
            "stop"
        };
        let data = json!({
            "role": "assistant", "time": {"created": millis, "completed": millis}, "parentID": parent,
            "modelId": model, "providerId": provider, "mode": "build", "agent": "zcode-agent",
            "path": {"cwd": self.cwd, "root": self.cwd}, "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            "finish": finish,
            "semantics": {"origin": "agent_runtime", "kind": "assistant_response", "uiVisibility": "visible", "providerVisibility": "visible", "transcriptVisibility": "visible"},
        });
        self.messages.push(Message {
            id,
            created: millis,
            updated: millis,
            data,
            parts,
        });
    }

    fn store_result(&mut self, event: &Event, result: &ToolResult) -> Result<()> {
        match self.calls.result(result) {
            Placement::InPlace { message, part } => {
                self.settle_result(message, part, event, result, None)
            }
            Placement::Synthetic(call) => {
                let synthetic = tool_order::synthetic(result, call);
                let millis = time::unix_millis(&event.timestamp);
                let id = sortable_id("msg", millis);
                let part = part(
                    millis,
                    json!({"type": "tool", "callID": synthetic.id, "tool": synthetic.name, "blockIndex": 0, "metadata": {}, "state": {"status": "pending", "input": synthetic.input, "raw": ""}}),
                );
                let message = self.messages.len();
                self.push_assistant(id, millis, vec![part], event);
                self.settle_result(message, 0, event, result, Some(synthetic.marker))
            }
        }
    }

    fn settle_result(
        &mut self,
        message: usize,
        index: usize,
        event: &Event,
        result: &ToolResult,
        mut marker: Option<Value>,
    ) -> Result<()> {
        let row = &self.messages[message];
        let start = row.created;
        let end = time::unix_millis(&event.timestamp).max(start);
        let call = &row.parts[index].data;
        let tool = call["tool"].clone();
        let input = call["state"]["input"].clone();
        let mut state = if result.is_error {
            if !result.images.is_empty() {
                let mut attached = Vec::new();
                for image in &result.images {
                    attached.push(images::data_url(self.blobs, image)?);
                }
                marker.get_or_insert_with(|| json!({}))["images"] = json!(attached);
            }
            json!({"status": "error", "input": input, "error": result.text, "metadata": {}, "time": {"start": start, "end": end}})
        } else {
            let mut state = json!({"status": "completed", "input": input, "output": result.text, "title": tool, "metadata": {}, "time": {"start": start, "end": end}});
            if !result.images.is_empty() {
                let mut attachments = Vec::new();
                let mut layout = Vec::new();
                if !result.text.is_empty() {
                    layout.push(json!({"type": "text", "text": result.text}));
                }
                for (index, image) in result.images.iter().enumerate() {
                    let mut file = self.file(image)?;
                    file["id"] = json!(sortable_id("part", end));
                    file["sessionID"] = json!(self.session);
                    file["messageID"] = json!(row.id);
                    attachments.push(file);
                    layout.push(json!({"type": "attachment", "attachmentIndex": index}));
                }
                state["attachments"] = json!(attachments);
                // ZCode only replays attachment media listed in this layout
                state["metadata"]["modelContentLayout"] = json!(layout);
            }
            state
        };
        tool_order::set_marker(&mut state, marker);
        self.messages[message].parts[index].data["state"] = state;
        Ok(())
    }

    fn file(&self, image: &Image) -> Result<Value> {
        Ok(json!({
            "type": "file", "mime": image.mime,
            "filename": format!("image.{}", images::extension(&image.mime)),
            "url": images::data_url(self.blobs, image)?,
            "metadata": {"storageKind": "inline", "recoverability": "provider_ready"},
        }))
    }
}

fn part(millis: i64, data: Value) -> Part {
    Part {
        id: sortable_id("part", millis),
        created: millis,
        data,
    }
}

fn sortable_id(prefix: &str, millis: i64) -> String {
    let mut value = millis.max(0) as u64;
    let mut digits = Vec::new();
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize] as char);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    let timestamp: String = digits.into_iter().rev().collect();
    format!("{prefix}_{timestamp}_{}", identifiers::uuid_v4())
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut invalid = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-') {
            slug.push(ch);
            invalid = false;
        } else if !invalid {
            slug.push('-');
            invalid = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug.to_owned()
    }
}

fn title_from_input(value: &str) -> String {
    let title = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return "Untitled session".to_owned();
    }
    if title.encode_utf16().count() <= 60 {
        return title;
    }
    let mut end = 0;
    let mut units = 0;
    for ch in title.chars() {
        if units + ch.len_utf16() > 57 {
            break;
        }
        units += ch.len_utf16();
        end += ch.len_utf8();
    }
    format!("{}...", &title[..end])
}
