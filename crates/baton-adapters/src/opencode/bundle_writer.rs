use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Blobs, Body, Event, Export, Image, ToolResult, time};
use serde_json::{Value, json};

use crate::opencode::ascending_ids::AscendingIds;
use crate::support::images;
use crate::support::unit::{self, Unit};

// OpenCode's placeholder title, which it replaces with a generated one after the next message
pub(super) const UNNAMED: &str = "New session - ";

// OpenCode keeps a tool result inside the part of its call, so a result whose call is missing is left out
pub(super) fn write_bundle(
    export: &Export,
    session_id: &str,
    version: &str,
    blobs: &Blobs,
) -> Result<Value> {
    let cwd = export.cwd.to_string_lossy().into_owned();
    let mut writer = BundleWriter {
        blobs,
        session: session_id,
        cwd: &cwd,
        ids: AscendingIds::default(),
        messages: Vec::new(),
        calls: HashMap::new(),
        last_user: None,
    };
    writer.push_events(export.events)?;
    let created = time::unix_millis(export.created);
    let mut updated = created;
    for message in &writer.messages {
        if let Some(millis) = message["info"]["time"]["created"].as_i64() {
            updated = updated.max(millis);
        }
    }
    let title = match export.title {
        Some(title) => title.to_owned(),
        None => format!("{UNNAMED}{}", export.created),
    };
    Ok(json!({
        "info": {
            "id": session_id,
            "slug": format!("baton-{}", &session_id[session_id.len() - 8..]),
            "projectID": "global",
            "directory": cwd,
            "title": title,
            "version": version,
            "time": {"created": created, "updated": updated},
        },
        "messages": writer.messages,
    }))
}

enum Reply {
    Turn,
    CompactionSummary,
}

// Where a tool call's part sits, so its result can be stored into it
struct CallPart {
    message: usize,
    part: usize,
}

struct BundleWriter<'a> {
    blobs: &'a Blobs,
    session: &'a str,
    cwd: &'a str,
    ids: AscendingIds,
    messages: Vec<Value>,
    calls: HashMap<String, CallPart>,
    last_user: Option<String>,
}

impl BundleWriter<'_> {
    fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    let millis = time::unix_millis(&event.timestamp);
                    let id = self.ids.next("msg", millis);
                    let mut parts = Vec::new();
                    if !user.text.is_empty() {
                        parts.push(self.part(
                            &id,
                            millis,
                            json!({"type": "text", "text": user.text}),
                        ));
                    }
                    for image in &user.images {
                        let file = self.file(image)?;
                        parts.push(self.part(&id, millis, file));
                    }
                    self.push_user(&id, millis, parts);
                }
                Unit::Assistant(events) => {
                    let millis = time::unix_millis(&events[0].timestamp);
                    let id = self.ids.next("msg", millis);
                    let mut parts = Vec::new();
                    for event in &events {
                        let fields = match &event.body {
                            Body::Assistant(assistant) => {
                                json!({"type": "text", "text": assistant.text})
                            }
                            Body::ToolCall(call) => {
                                let position = CallPart {
                                    message: self.messages.len(),
                                    part: parts.len(),
                                };
                                self.calls.insert(call.call_id.clone(), position);
                                json!({"type": "tool", "callID": call.call_id, "tool": call.name,
                                       "state": {"status": "pending", "input": call.input, "raw": ""}})
                            }
                            _ => continue,
                        };
                        parts.push(self.part(&id, millis, fields));
                    }
                    let parent = self.last_user.clone().unwrap_or_else(|| id.clone());
                    self.push_assistant(&id, &parent, millis, parts, Reply::Turn);
                }
                Unit::ToolResult(event, result) => self.store_result(event, result)?,
                Unit::Compaction(event, summary) => {
                    let millis = time::unix_millis(&event.timestamp);
                    let request = self.ids.next("msg", millis);
                    let marker = self.part(
                        &request,
                        millis,
                        json!({"type": "compaction", "auto": true}),
                    );
                    self.push_user(&request, millis, vec![marker]);
                    let id = self.ids.next("msg", millis);
                    let text = self.part(&id, millis, json!({"type": "text", "text": summary}));
                    self.push_assistant(
                        &id,
                        &request,
                        millis,
                        vec![text],
                        Reply::CompactionSummary,
                    );
                }
            }
        }
        Ok(())
    }

    fn store_result(&mut self, event: &Event, result: &ToolResult) -> Result<()> {
        let Some(position) = self.calls.get(&result.call_id) else {
            return Ok(());
        };
        let (message, index) = (position.message, position.part);
        let info = &self.messages[message]["info"];
        let start = info["time"]["created"].as_i64().unwrap_or_default();
        let message_id = info["id"].as_str().unwrap_or_default().to_owned();
        let end = time::unix_millis(&event.timestamp).max(start);
        let call = &self.messages[message]["parts"][index];
        let tool = call["tool"].clone();
        let input = call["state"]["input"].clone();
        let state = if result.is_error {
            json!({"status": "error", "input": input, "error": result.text, "time": {"start": start, "end": end}})
        } else {
            let mut state = json!({"status": "completed", "input": input, "output": result.text, "title": tool,
                                   "metadata": {}, "time": {"start": start, "end": end}});
            if !result.images.is_empty() {
                let mut attachments = Vec::new();
                for image in &result.images {
                    let file = self.file(image)?;
                    attachments.push(self.part(&message_id, end, file));
                }
                state["attachments"] = json!(attachments);
            }
            state
        };
        self.messages[message]["parts"][index]["state"] = state;
        Ok(())
    }

    fn part(&mut self, message: &str, millis: i64, fields: Value) -> Value {
        let mut part = json!({"id": self.ids.next("prt", millis), "sessionID": self.session, "messageID": message});
        for (key, value) in fields.as_object().expect("parts are objects") {
            part[key] = value.clone();
        }
        part
    }

    fn file(&self, image: &Image) -> Result<Value> {
        Ok(json!({
            "type": "file",
            "mime": image.mime,
            "filename": format!("image.{}", images::extension(&image.mime)),
            "url": images::data_url(self.blobs, image)?,
        }))
    }

    fn push_user(&mut self, id: &str, millis: i64, parts: Vec<Value>) {
        let info = json!({"id": id, "sessionID": self.session, "role": "user", "time": {"created": millis},
                          "agent": "build", "model": {"providerID": "baton", "modelID": "baton"}});
        self.messages.push(json!({"info": info, "parts": parts}));
        self.last_user = Some(id.to_owned());
    }

    fn push_assistant(
        &mut self,
        id: &str,
        parent: &str,
        millis: i64,
        parts: Vec<Value>,
        reply: Reply,
    ) {
        let finish = if parts.iter().any(|part| part["type"] == "tool") {
            "tool-calls"
        } else {
            "stop"
        };
        let mut info = json!({
            "id": id, "sessionID": self.session, "role": "assistant", "time": {"created": millis, "completed": millis},
            "parentID": parent, "modelID": "baton", "providerID": "baton", "mode": "build", "agent": "build",
            "path": {"cwd": self.cwd, "root": self.cwd}, "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            "finish": finish,
        });
        if let Reply::CompactionSummary = reply {
            info["summary"] = json!(true);
        }
        self.messages.push(json!({"info": info, "parts": parts}));
    }
}
