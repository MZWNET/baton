use std::collections::HashMap;

use anyhow::Result;
use baton_core::{Body, Event, ToolCall, ToolResult, User};
use serde_json::{Value, json};

use crate::support::normalize;

pub(crate) fn calls_by_id<'a>(events: &[&'a Event]) -> HashMap<&'a str, &'a ToolCall> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            Body::ToolCall(call) => Some((call.call_id.as_str(), call)),
            _ => None,
        })
        .collect()
}

pub(crate) fn decode_function_call(call: &Value, id: &Value) -> Result<ToolCall> {
    let function = &call["function"];
    Ok(ToolCall {
        call_id: normalize::call_id(id)?,
        name: function["name"].as_str().unwrap_or("unknown").to_owned(),
        input: normalize::arguments(&function["arguments"]),
    })
}

pub(crate) fn encode_function_call(call: &ToolCall) -> Value {
    json!({"id": call.call_id, "type": "function", "function": {"name": call.name, "arguments": call.input.to_string()}})
}

// One native message per run of replayed events, the way chat transcripts group them
pub(crate) enum Unit<'a> {
    User(&'a Event, &'a User),
    // Text blocks and tool calls the assistant produced in one turn
    Assistant(Vec<&'a Event>),
    ToolResult(&'a Event, &'a ToolResult),
    Compaction(&'a Event, &'a str),
}

pub(crate) fn units<'a>(events: &[&'a Event]) -> Vec<Unit<'a>> {
    let mut units = Vec::new();
    for &event in events {
        match &event.body {
            Body::User(user) => units.push(Unit::User(event, user)),
            Body::Assistant(_) | Body::ToolCall(_) => match units.last_mut() {
                Some(Unit::Assistant(run)) => run.push(event),
                _ => units.push(Unit::Assistant(vec![event])),
            },
            Body::ToolResult(result) => units.push(Unit::ToolResult(event, result)),
            Body::Compaction(compaction) => {
                units.push(Unit::Compaction(event, &compaction.summary))
            }
            Body::Thinking(_) | Body::Meta(_) => {}
        }
    }
    units
}

// An assistant message of a format that holds a single text per message, with the calls that followed the text
pub(crate) struct Reply<'a> {
    pub(crate) event: &'a Event,
    // Empty when the run starts with a call
    pub(crate) text: &'a str,
    pub(crate) calls: Vec<&'a ToolCall>,
}

// Every text block of an assistant run starts a reply, so text and calls keep their order
pub(crate) fn replies<'a>(run: &[&'a Event]) -> Vec<Reply<'a>> {
    let mut replies: Vec<Reply> = Vec::new();
    for &event in run {
        match &event.body {
            Body::Assistant(assistant) => replies.push(Reply {
                event,
                text: &assistant.text,
                calls: Vec::new(),
            }),
            Body::ToolCall(call) => match replies.last_mut() {
                Some(reply) => reply.calls.push(call),
                None => replies.push(Reply {
                    event,
                    text: "",
                    calls: vec![call],
                }),
            },
            _ => {}
        }
    }
    replies
}

// Texts the assistant wrote and the calls that followed them, for formats that keep the texts and the calls of a message apart
pub(crate) struct Segment<'a> {
    pub(crate) event: &'a Event,
    pub(crate) texts: Vec<&'a str>,
    pub(crate) calls: Vec<&'a ToolCall>,
}

// A text after a call starts a new segment, so texts and calls keep their order
pub(crate) fn segments<'a>(run: &[&'a Event]) -> Vec<Segment<'a>> {
    let mut segments: Vec<Segment> = Vec::new();
    for &event in run {
        match &event.body {
            Body::Assistant(assistant) => match segments.last_mut() {
                Some(segment) if segment.calls.is_empty() => segment.texts.push(&assistant.text),
                _ => segments.push(Segment {
                    event,
                    texts: vec![&assistant.text],
                    calls: Vec::new(),
                }),
            },
            Body::ToolCall(call) => match segments.last_mut() {
                Some(segment) => segment.calls.push(call),
                None => segments.push(Segment {
                    event,
                    texts: Vec::new(),
                    calls: vec![call],
                }),
            },
            _ => {}
        }
    }
    segments
}
