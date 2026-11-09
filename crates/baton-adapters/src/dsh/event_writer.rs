use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use baton_core::{Blobs, Body, Event, Image, ToolCall, ToolResult, time};
use serde_json::{Value, json};

use super::event_parser::{BATON_PROVIDER, SYNTHETIC_PREFIX};
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, json};

const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.\n\n<compacted-summary>";
const INTERRUPTED: &str = "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed.";

pub(super) struct EventWriter<'a> {
    blobs: &'a Blobs,
    objects_dir: PathBuf,
    header: String,
    events: String,
    seq: usize,
    time: i64,
    turn: usize,
    step: usize,
    turn_open: bool,
    step_open: bool,
    surface: Vec<usize>,
}

impl<'a> EventWriter<'a> {
    pub(super) fn new(
        blobs: &'a Blobs,
        objects_dir: &Path,
        id: &str,
        cwd: &str,
        created: &str,
    ) -> Self {
        let mut header = String::new();
        let created = time::unix_millis(created);
        json::push_line(
            &mut header,
            &json!({"type": "session", "version": 4, "id": id, "createdAt": created, "cwd": cwd, "isSeeded": false, "delegationDepth": 0}),
        );
        Self {
            blobs,
            objects_dir: objects_dir.to_owned(),
            header,
            events: String::new(),
            seq: 0,
            time: created,
            turn: 0,
            step: 0,
            turn_open: false,
            step_open: false,
            surface: Vec::new(),
        }
    }

    pub(super) fn push_events(&mut self, events: &[&Event]) -> Result<()> {
        let calls = unit::calls_by_id(events);
        let mut pending = Vec::new();
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    self.advance(&event.timestamp);
                    self.close_turn(&mut pending);
                    self.open_turn();
                    let content = self.content(&user.text, &user.images)?;
                    self.append_surface(
                        "user/message",
                        json!({"id": identifiers::uuid_v4(), "role": "user", "source": {"kind": "user"}, "content": content}),
                    );
                }
                Unit::Assistant(run) => {
                    self.advance(&run[0].timestamp);
                    self.close_step(&mut pending);
                    self.open_step();
                    let mut blocks = Vec::with_capacity(run.len());
                    for event in run {
                        match &event.body {
                            Body::Assistant(assistant) => {
                                blocks.push(json!({"type": "text", "text": assistant.text}))
                            }
                            Body::ToolCall(call) => {
                                blocks.push(call_block(call));
                                pending.push(call);
                            }
                            _ => unreachable!(),
                        }
                    }
                    self.assistant_message(identifiers::uuid_v4(), blocks, !pending.is_empty());
                }
                Unit::ToolResult(event, result) => {
                    self.advance(&event.timestamp);
                    let position = pending
                        .iter()
                        .position(|call| call.call_id == result.call_id);
                    if let Some(position) = position {
                        let call = pending.remove(position);
                        self.tool_result(call, result)?;
                    } else {
                        self.close_step(&mut pending);
                        self.open_step();
                        let synthetic;
                        let call = if let Some(call) = calls.get(result.call_id.as_str()) {
                            *call
                        } else {
                            synthetic = ToolCall {
                                call_id: result.call_id.clone(),
                                name: "unknown".into(),
                                input: json!({}),
                            };
                            &synthetic
                        };
                        self.assistant_message(
                            format!("{SYNTHETIC_PREFIX}{}", identifiers::uuid_v4()),
                            vec![call_block(call)],
                            true,
                        );
                        self.tool_result(call, result)?;
                    }
                }
                Unit::Compaction(event, summary) => {
                    self.advance(&event.timestamp);
                    self.close_turn(&mut pending);
                    self.compaction(summary);
                }
            }
        }
        self.close_turn(&mut pending);
        Ok(())
    }

    pub(super) fn push_title(&mut self, timestamp: &str, title: &str) {
        self.advance(timestamp);
        self.push(
            "session/title",
            json!({"title": title, "messageSeqs": [], "source": {"kind": "user"}}),
            None,
            None,
        );
    }

    pub(super) fn finish(self) -> (String, String) {
        (self.header, self.events)
    }

    fn advance(&mut self, timestamp: &str) {
        self.time = time::unix_millis(timestamp).max(self.time + 1);
    }

    fn push(
        &mut self,
        kind: &str,
        data: Value,
        surface_op: Option<Value>,
        source_seqs: Option<Vec<usize>>,
    ) -> usize {
        let seq = self.seq;
        self.seq += 1;
        let mut event = json!({"type": kind, "seq": seq, "time": self.time, "data": data});
        if let Some(op) = surface_op {
            event["surfaceOp"] = op;
        }
        if let Some(seqs) = source_seqs {
            event["sourceEventSeqs"] = json!(seqs);
        }
        json::push_line(&mut self.events, &event);
        seq
    }

    fn append_surface(&mut self, kind: &str, data: Value) {
        let seq = self.push(kind, data, Some(json!("append")), None);
        self.surface.push(seq);
    }

    fn open_turn(&mut self) {
        if !self.turn_open {
            self.turn += 1;
            self.step = 0;
            self.turn_open = true;
            self.push("turn/start", json!({"turn": self.turn}), None, None);
        }
    }

    fn open_step(&mut self) {
        self.open_turn();
        self.step += 1;
        self.step_open = true;
        self.push(
            "step/start",
            json!({"turn": self.turn, "step": self.step}),
            None,
            None,
        );
    }

    fn close_step(&mut self, pending: &mut Vec<&ToolCall>) {
        if self.step_open {
            for call in pending.drain(..) {
                let id = format!("interrupted-tool-result-{}-{}", call.call_id, self.seq);
                let message = json!({"id": id, "role": "tool", "toolCallId": call.call_id, "isError": true, "source": {"kind": "tool", "callId": call.call_id}, "content": [{"type": "text", "text": INTERRUPTED}]});
                self.append_surface(
                    "tool/result",
                    json!({"turn": self.turn, "step": self.step, "message": message, "error": {"name": "ToolNotStartedError", "code": "TOOL_NOT_STARTED"}}),
                );
            }
            self.push(
                "step/end",
                json!({"turn": self.turn, "step": self.step}),
                None,
                None,
            );
            self.step_open = false;
        }
    }

    fn close_turn(&mut self, pending: &mut Vec<&ToolCall>) {
        self.close_step(pending);
        if self.turn_open {
            self.push(
                "turn/end",
                json!({"turn": self.turn, "reason": {"kind": "completed"}}),
                None,
                None,
            );
            self.turn_open = false;
        }
    }

    fn assistant_message(&mut self, id: String, blocks: Vec<Value>, has_calls: bool) {
        let mut stream: Vec<Value> = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                json!({"type": "chunk", "time": self.time, "chunk": {"type": "block-end", "index": index, "block": block}})
            })
            .collect();
        let reason = if has_calls { "tool-calls" } else { "stop" };
        stream.push(json!({"type": "chunk", "time": self.time, "chunk": {"type": "finish", "reason": {"kind": reason}}}));
        let message = json!({"id": id, "role": "assistant", "source": {"kind": "model", "provider": BATON_PROVIDER, "model": "baton"}, "content": blocks});
        self.append_surface(
            "assistant/message",
            json!({"turn": self.turn, "step": self.step, "message": message, "stream": stream}),
        );
    }

    fn tool_result(&mut self, call: &ToolCall, result: &ToolResult) -> Result<()> {
        let content = self.content(&result.text, &result.images)?;
        self.push(
            "tool/call",
            json!({"turn": self.turn, "step": self.step, "callId": call.call_id, "name": call.name, "arguments": call.input.to_string()}),
            None,
            None,
        );
        let message = json!({"id": identifiers::uuid_v4(), "role": "tool", "source": {"kind": "tool", "callId": result.call_id}, "toolCallId": result.call_id, "content": content, "isError": result.is_error});
        self.append_surface(
            "tool/result",
            json!({"turn": self.turn, "step": self.step, "message": message}),
        );
        Ok(())
    }

    fn content(&self, text: &str, attached: &[Image]) -> Result<Vec<Value>> {
        let mut content = vec![json!({"type": "text", "text": text})];
        for image in attached {
            let bytes = self.blobs.get(&image.blob)?;
            let path = self.objects_dir.join(&image.blob[..2]).join(&image.blob);
            let dir = path.parent().expect("attachment path has a directory");
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => file
                    .write_all(&bytes)
                    .with_context(|| format!("writing {}", path.display()))?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
            let (width, height) = dimensions(&bytes).unwrap_or((0, 0));
            content.push(json!({"type": "image", "attachment": {"attachmentId": format!("sha256:{}", image.blob), "mediaType": image.mime, "bytes": bytes.len(), "width": width, "height": height}}));
        }
        Ok(content)
    }

    fn compaction(&mut self, summary: &str) {
        let id = identifiers::uuid_v4();
        let message = json!({"id": identifiers::uuid_v4(), "role": "user", "source": {"kind": "compact-checkpoint", "compactionId": id}, "content": [{"type": "text", "text": CHECKPOINT_PREAMBLE}, {"type": "text", "text": summary}, {"type": "text", "text": "</compacted-summary>"}]});
        if self.surface.is_empty() {
            self.append_surface("user/message", message);
            return;
        }

        let start = self.push(
            "compaction/start",
            json!({"compactionId": id, "turn": null}),
            None,
            None,
        );
        let first = self.surface[0];
        let last = *self.surface.last().expect("surface is not empty");
        let summary_seq = self.push(
            "compaction/summary",
            json!({"compactionId": id, "summary": [{"type": "text", "text": summary}], "shadowedRange": {"start": first, "end": last}, "shadowedSeqs": self.surface, "shadowedTokenCount": 0, "provider": BATON_PROVIDER, "model": "baton"}),
            None,
            None,
        );
        let mut sources = vec![start, summary_seq];
        sources.extend(&self.surface);
        let op = json!({"op": "replace", "startSeq": first, "endSeq": last});
        let checkpoint = self.push("user/message", message, Some(op), Some(sources));
        self.surface.clear();
        self.surface.push(checkpoint);
        self.push(
            "compaction/end",
            json!({"compactionId": id, "turn": null}),
            None,
            None,
        );
    }
}

fn call_block(call: &ToolCall) -> Value {
    json!({"type": "tool-call", "id": call.call_id, "name": call.name, "arguments": call.input.to_string()})
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be_u16 =
        |offset| Some(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32);
    let le_u16 =
        |offset| Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32);

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);

        Some((width, height))
    } else if bytes.starts_with(b"GIF8") {
        Some((le_u16(6)?, le_u16(8)?))
    } else if bytes.starts_with(b"\xff\xd8") {
        let mut offset = 2;
        loop {
            if *bytes.get(offset)? != 0xff {
                return None;
            }
            while *bytes.get(offset)? == 0xff {
                offset += 1;
            }
            let marker = *bytes.get(offset)?;
            offset += 1;
            match marker {
                0xd9 | 0xda => return None,
                0x01 | 0xd0..=0xd8 => continue,
                _ => {}
            }
            let length = be_u16(offset)? as usize;
            if length < 2 || bytes.get(offset..offset + length).is_none() {
                return None;
            }
            if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                if length < 7 {
                    return None;
                }

                return Some((be_u16(offset + 5)?, be_u16(offset + 3)?));
            }
            offset += length;
        }
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        match bytes.get(12..16)? {
            b"VP8 " => Some((le_u16(26)? & 0x3fff, le_u16(28)? & 0x3fff)),
            b"VP8L" if *bytes.get(20)? == 0x2f => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);

                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => {
                let width = bytes.get(24..27)?;
                let height = bytes.get(27..30)?;

                Some((
                    u32::from_le_bytes([width[0], width[1], width[2], 0]) + 1,
                    u32::from_le_bytes([height[0], height[1], height[2], 0]) + 1,
                ))
            }
            _ => None,
        }
    } else {
        None
    }
}
