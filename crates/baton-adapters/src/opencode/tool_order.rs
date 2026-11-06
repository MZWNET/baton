use std::collections::{HashMap, VecDeque};

use baton_core::{Body, Event, ToolCall, ToolResult};
use serde_json::{Value, json};

use crate::support::identifiers;

#[derive(Default)]
pub(super) struct ToolOrder<'a> {
    calls: HashMap<&'a str, (&'a ToolCall, usize, usize)>,
    pending: VecDeque<&'a str>,
}

pub(super) enum Placement<'a> {
    InPlace { message: usize, part: usize },
    Synthetic(Option<&'a ToolCall>),
}

pub(super) struct Synthetic<'a> {
    pub(super) id: String,
    pub(super) name: &'a str,
    pub(super) input: Value,
    pub(super) marker: Value,
}

impl<'a> ToolOrder<'a> {
    pub(super) fn insert(&mut self, call: &'a ToolCall, message: usize, part: usize) {
        self.calls.insert(&call.call_id, (call, message, part));
    }

    pub(super) fn assistant(&mut self, events: &[&'a Event]) {
        self.pending = events
            .iter()
            .filter_map(|&event| match &event.body {
                Body::ToolCall(call) => Some(call.call_id.as_str()),
                _ => None,
            })
            .collect();
    }

    pub(super) fn clear(&mut self) {
        self.pending.clear();
    }

    pub(super) fn result(&mut self, result: &ToolResult) -> Placement<'a> {
        let call = self.calls.get(result.call_id.as_str()).copied();
        if self.pending.front().copied() == Some(result.call_id.as_str()) {
            let _ = self.pending.pop_front();
            let (_, message, part) = call.expect("pending tool calls are recorded");
            Placement::InPlace { message, part }
        } else {
            self.pending.clear();
            Placement::Synthetic(call.map(|(call, _, _)| call))
        }
    }
}

pub(super) fn synthetic<'a>(result: &ToolResult, call: Option<&'a ToolCall>) -> Synthetic<'a> {
    Synthetic {
        id: format!("baton-synthetic-{}", identifiers::uuid_v4()),
        name: call.map_or("unknown", |call| call.name.as_str()),
        input: call.map_or_else(|| json!({}), |call| call.input.clone()),
        marker: json!({"synthetic": true, "call_id": result.call_id}),
    }
}

pub(super) fn set_marker(state: &mut Value, marker: Option<Value>) {
    if let Some(marker) = marker {
        state["metadata"]["baton"] = marker;
    }
}

pub(super) fn synthetic_call_id(state: &Value) -> Option<&str> {
    let marker = &state["metadata"]["baton"];
    (marker["synthetic"] == true)
        .then(|| marker["call_id"].as_str())
        .flatten()
}
