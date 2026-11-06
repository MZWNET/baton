use anyhow::Result;
use baton_core::{Blobs, Event, Image, ToolCall, time};
use serde_json::Value;

use crate::antigravity::conversation::{
    BATON_FIELD, DONE, ERROR, GENERIC, PLANNER_RESPONSE, USER_INPUT,
};
use crate::support::identifiers;
use crate::support::protobuf::Encoder;
use crate::support::unit::{self, Unit};

// A row of the `steps` table; its metadata goes both into a column of its own and into the step
pub(super) struct Step {
    pub(super) kind: u64,
    pub(super) status: u64,
    pub(super) metadata: Vec<u8>,
    pub(super) payload: Vec<u8>,
    pub(super) millis: i64,
}

// Builds the steps the way agy writes them: a planner step per text with the calls after it, a generic step per result linked to its call through the metadata, and the steps of a user turn sharing one execution id
pub(super) fn steps(events: &[&Event], blobs: &Blobs) -> Result<Vec<Step>> {
    let calls = unit::calls_by_id(events);
    let mut writer = StepWriter {
        execution: identifiers::uuid_v4(),
        steps: Vec::new(),
    };
    for unit in unit::units(events) {
        match unit {
            Unit::User(event, user) => {
                let input = Encoder::default().string(2, &user.text);
                let baton = images(&user.images, blobs)?;
                writer.push(event, USER_INPUT, DONE, None, input, baton);
            }
            Unit::Assistant(run) => {
                for reply in unit::replies(&run) {
                    let mut planner = Encoder::default();
                    if !reply.text.is_empty() {
                        planner = planner.string(1, reply.text);
                    }
                    planner = planner.string(6, &format!("bot-{}", identifiers::uuid_v4()));
                    for call in &reply.calls {
                        planner = planner.message(7, tool_call(call));
                    }
                    writer.push(
                        reply.event,
                        PLANNER_RESPONSE,
                        DONE,
                        None,
                        planner,
                        Encoder::default(),
                    );
                }
            }
            Unit::ToolResult(event, result) => {
                let call = calls.get(result.call_id.as_str());
                let answered = match call {
                    Some(call) => tool_call(call),
                    None => Encoder::default().string(1, &result.call_id),
                };
                // The arguments again, one entry per top-level key with strings left unquoted, as agy lists them
                let mut generic = Encoder::default();
                if let Some(Value::Object(input)) = call.map(|call| &call.input) {
                    for (key, value) in input {
                        let value = match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        generic =
                            generic.message(1, Encoder::default().string(1, key).string(2, &value));
                    }
                }
                let generic = generic.message(2, Encoder::default().string(1, &result.text));
                let status = if result.is_error { ERROR } else { DONE };
                let baton = images(&result.images, blobs)?;
                writer.push(event, GENERIC, status, Some(answered), generic, baton);
            }
            // Antigravity has no compaction record, so a summary is a marked user input that agy sees after the full history
            Unit::Compaction(event, summary) => {
                let input = Encoder::default().string(2, summary);
                writer.push(
                    event,
                    USER_INPUT,
                    DONE,
                    None,
                    input,
                    Encoder::default().varint(2, 1),
                );
            }
        }
    }
    Ok(writer.steps)
}

pub(super) fn timestamp(millis: i64) -> Encoder {
    let seconds = millis.div_euclid(1000) as u64;
    let nanos = millis.rem_euclid(1000) as u64 * 1_000_000;
    let timestamp = Encoder::default().varint(1, seconds);
    if nanos == 0 {
        timestamp
    } else {
        timestamp.varint(2, nanos)
    }
}

fn tool_call(call: &ToolCall) -> Encoder {
    Encoder::default()
        .string(1, &call.call_id)
        .string(2, &call.name)
        .string(3, &call.input.to_string())
}

// Images go into baton's private field, as Antigravity keeps none of its own in a step
fn images(attached: &[Image], blobs: &Blobs) -> Result<Encoder> {
    let mut baton = Encoder::default();
    for image in attached {
        let bytes = blobs.get(&image.blob)?;
        baton = baton.message(
            1,
            Encoder::default().string(1, &image.mime).bytes(2, &bytes),
        );
    }
    Ok(baton)
}

struct StepWriter {
    // Shared by the steps of one user turn
    execution: String,
    steps: Vec<Step>,
}

impl StepWriter {
    fn push(
        &mut self,
        event: &Event,
        kind: u64,
        status: u64,
        answered: Option<Encoder>,
        payload: Encoder,
        baton: Encoder,
    ) {
        let (field, source) = match kind {
            USER_INPUT => {
                self.execution = identifiers::uuid_v4();
                (19, 4)
            }
            PLANNER_RESPONSE => (20, 2),
            _ => (140, 2),
        };
        let millis = time::unix_millis(&event.timestamp);
        let mut metadata = Encoder::default()
            .message(1, timestamp(millis))
            .varint(3, source);
        if let Some(answered) = answered {
            metadata = metadata.message(4, answered);
        }
        let metadata = metadata.string(12, &self.execution).finish();
        let baton = baton.finish();
        let mut step = Encoder::default()
            .varint(1, kind)
            .varint(4, status)
            .bytes(5, &metadata)
            .message(field, payload);
        if !baton.is_empty() {
            step = step.bytes(BATON_FIELD, &baton);
        }
        self.steps.push(Step {
            kind,
            status,
            metadata,
            payload: step.finish(),
            millis,
        });
    }
}
