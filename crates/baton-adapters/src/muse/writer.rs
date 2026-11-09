use std::collections::HashMap;

use baton_core::{Body, Event, time};
use serde_json::{Value, json};

use crate::muse::markers;
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, json};

// The Muse build written into the log; newer builds refuse sessions an even newer one recorded
const BUILD: (&str, &str) = ("0.2.1", "b3170a534f");

// A run of the turn being written, which every assistant event has to belong to
struct Run {
    id: String,
    sequence: u64,
}

// Builds a Muse session log with the lifecycle Muse rebuilds the model's context from: every user turn is an accepted intent, a started run and its materialization
pub(super) struct LogWriter<'a> {
    session: &'a str,
    out: String,
    sequence: u64,
    micros: i64,
    turns: usize,
    run: Option<Run>,
    // The message of the event that made each call, which its result batch refers to
    batches: HashMap<&'a str, String>,
}

impl<'a> LogWriter<'a> {
    pub(super) fn new(session: &'a str, created: &str, cwd: &str) -> LogWriter<'a> {
        let mut writer = LogWriter {
            session,
            out: String::new(),
            sequence: 0,
            micros: 0,
            turns: 0,
            run: None,
            batches: HashMap::new(),
        };
        let metadata = json!({"kind": "metadata", "record": {"build": {"semver": BUILD.0, "sha": BUILD.1}, "model_id": "baton",
                              "provider_id": "meta", "tool_surface_version": "1", "web_search_mode": "off", "workspace_root": cwd}});
        writer.push(created, "runtime.session.metadata", metadata);
        let opened = json!({"kind": "session_opened", "record": {"data_dir_free_bytes": 0, "resume": true, "schema_version": 1,
                            "security_mode": "normal", "session_id": session, "workspace_free_bytes": 0}});
        writer.push(created, "session.opened.observed", opened);
        writer
    }

    pub(super) fn push_events(&mut self, events: &[&'a Event]) {
        for unit in unit::units(events) {
            match unit {
                Unit::User(event, user) => {
                    self.push_turn(&event.timestamp, &markers::encode(&user.text, &user.images));
                }
                Unit::Assistant(run) => {
                    for event in run {
                        match &event.body {
                            Body::Assistant(assistant) => {
                                let id = identifiers::uuid_v4();
                                let committed = json!({"kind": "assistant_message_committed", "message_id": id, "provider_item_id": id,
                                                       "response_id": identifiers::uuid_v4(), "text": assistant.text});
                                self.push_run_event(&event.timestamp, committed);
                            }
                            Body::ToolCall(call) => {
                                let id = identifiers::uuid_v4();
                                let calls = [
                                    json!({"args": call.input.to_string(), "call_id": call.call_id, "id": call.call_id, "name": call.name}),
                                ];
                                let committed = json!({"kind": "assistant_tool_calls_committed", "message_id": id,
                                                       "response_id": identifiers::uuid_v4(), "tool_calls": calls});
                                self.push_run_event(&event.timestamp, committed);
                                self.batches.insert(&call.call_id, id);
                            }
                            _ => {}
                        }
                    }
                }
                Unit::ToolResult(event, result) => {
                    let batch = self
                        .batches
                        .get(result.call_id.as_str())
                        .cloned()
                        .unwrap_or_else(identifiers::uuid_v4);
                    let results = [
                        json!({"text": markers::encode_result(&result.text, &result.images, result.is_error), "tool_call_id": result.call_id, "tool_call_index": 0}),
                    ];
                    let committed = json!({"batch_id": batch, "kind": "tool_result_batch_committed", "results": results});
                    self.push_run_event(&event.timestamp, committed);
                }
                // Muse sees the summary as a user message, with the turns before it still in its context
                Unit::Compaction(event, summary) => {
                    self.push_turn(&event.timestamp, &markers::summary(summary));
                }
            }
        }
    }

    pub(super) fn finish(mut self) -> String {
        let timestamp = time::from_unix_millis(self.micros / 1000).unwrap_or_else(time::now);
        self.close(&timestamp);
        let end = json!({"kind": "session_end", "record": {"exit_reason": "clean", "resource_usage": {}, "schema_version": 1,
                         "session_id": self.session, "uptime_ms": 0}});
        self.push(&timestamp, "session.end", end);
        self.out
    }

    fn push_turn(&mut self, timestamp: &str, prompt: &str) {
        self.close(timestamp);
        let run = identifiers::uuid_v4();
        let blocks = [json!({"kind": "text", "text": prompt})];
        let accepted = json!({"bindings": [], "delivery_policy": "session_current", "intent_id": run,
                              "model_messages": [{"content": blocks}], "refill_blocks": blocks, "semantic_kind": {"kind": "chat"},
                              "source_session_id": self.session, "surface": "main", "wake_policy": "start_once"});
        let (accepted_id, accepted_sequence) =
            self.push(timestamp, "runtime.user_intent.accepted", accepted);
        let source = identifiers::uuid_v4();
        let started = json!({"event": {"kind": "started", "prompt": prompt}, "kind": "run", "run_id": run,
                             "source_run_record_id": source, "source_run_record_sequence": 1});
        let (started_id, _) = self.push(timestamp, "runtime.session", started);
        let outcome = json!({"cut_before": format!("session:{}:turn:{}", self.session, self.turns), "kind": "top_level_turn_started",
                             "run_id": run, "run_started_session_record_id": started_id, "run_started_source_record_id": source});
        let materialized = json!({"envelope_record_id": accepted_id, "envelope_sequence": accepted_sequence, "intent_id": run,
                                  "outcome": outcome, "source_session_id": self.session});
        self.push(timestamp, "runtime.user_intent.materialized", materialized);
        self.turns += 1;
        self.run = Some(Run {
            id: run,
            sequence: 1,
        });
    }

    // Events that come before any user message get a turn of their own, which the reader drops again
    fn push_run_event(&mut self, timestamp: &str, event: Value) {
        if self.run.is_none() {
            self.push_turn(timestamp, markers::CONTINUATION);
        }
        let run = self.run.as_mut().expect("a run was just opened");
        run.sequence += 1;
        let mut payload = json!({"event": event, "kind": "run", "run_id": run.id});
        if payload["event"]["kind"] != "tool_result_batch_committed" {
            payload["source_run_record_id"] = json!(identifiers::uuid_v4());
            payload["source_run_record_sequence"] = json!(run.sequence);
        }
        self.push(timestamp, "runtime.session", payload);
    }

    // Every run ends with a terminal event, which Muse resumes after
    fn close(&mut self, timestamp: &str) {
        if self.run.is_none() {
            return;
        }
        let terminal = json!({"eot_gate_ms": 0, "kind": "terminal", "reason": null, "terminal": "completed",
                              "time_to_first_token_ms": 0, "turn_duration_ms": 0});
        self.push_run_event(timestamp, terminal);
        self.run = None;
    }

    // Sequences have no gaps and record times strictly increase, or Muse refuses the log
    fn push(&mut self, timestamp: &str, kind: &str, payload: Value) -> (String, u64) {
        self.sequence += 1;
        self.micros = (time::unix_millis(timestamp) * 1000).max(self.micros + 1);
        let id = identifiers::uuid_v4();
        let envelope = json!({
            "causation_id": null, "durability": "durable", "id": id, "payload": payload, "payload_schema_version": 1,
            "payload_type": kind, "record_type": "event", "recorded_at": self.micros, "schema_version": 1,
            "sequence": self.sequence, "stream": {"id": self.session, "kind": "session"},
        });
        json::push_line(&mut self.out, &envelope);
        (id, self.sequence)
    }
}
