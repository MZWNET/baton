// The bundle conversions are tested directly because the adapter itself needs a real `opencode` command

use std::path::Path;

use baton_core::{Agent, Blobs, Body, Draft, Event, Export, ToolResult, replay};
use serde_json::{Value, json};

use crate::opencode::bundle_parser;
use crate::opencode::bundle_writer::{self, UNNAMED};

// What `opencode export` prints for a session with two steps, attachments, a failed and an unfinished tool, and a compaction
fn exported() -> Value {
    let part = |kind: &str, fields: Value| {
        let mut part = json!({"id": format!("prt_{kind}"), "sessionID": "ses_x", "messageID": "msg_x", "type": kind});
        for (key, value) in fields.as_object().unwrap() {
            part[key] = value.clone();
        }
        part
    };
    let message = |id: &str, role: &str, second: i64, parts: Vec<Value>| json!({"info": {"id": id, "role": role, "time": {"created": 1_787_054_400_000_i64 + second * 1000}}, "parts": parts});
    let mut summary = message(
        "msg_4",
        "assistant",
        3,
        vec![part("text", json!({"text": "compacted"}))],
    );
    summary["info"]["summary"] = json!(true);
    json!({
        "info": {"id": "ses_x", "directory": "/workspace/oc", "title": "OpenCode title", "time": {"created": 1_787_054_400_000_i64}},
        "messages": [
            message("msg_1", "user", 0, vec![
                part("text", json!({"text": "question"})),
                part("text", json!({"text": "ignored", "ignored": true})),
                part("file", json!({"mime": "image/png", "url": "data:image/png;base64,cGl4ZWxz"})),
                part("file", json!({"mime": "text/plain", "url": "data:text/plain;base64,eA=="})),
            ]),
            message("msg_2", "assistant", 1, vec![
                part("step-start", json!({})),
                part("reasoning", json!({"text": "thinking hard"})),
                part("tool", json!({"callID": "call_a", "tool": "read", "state": {"status": "completed", "input": {"path": "x"}, "output": "x body",
                    "attachments": [part("file", json!({"mime": "image/png", "url": "data:image/png;base64,cGl4ZWxz"}))]}})),
                part("step-finish", json!({})),
                part("step-start", json!({})),
                part("text", json!({"text": "x looks fine"})),
                part("tool", json!({"callID": "call_b", "tool": "bash", "state": {"status": "error", "input": {"command": "rm"}, "error": "denied"}})),
                part("tool", json!({"callID": "call_c", "tool": "bash", "state": {"status": "running", "input": {"command": "sleep"}}})),
            ]),
            message("msg_3", "user", 2, vec![part("compaction", json!({"auto": true}))]),
            summary,
            message("msg_5", "user", 4, vec![part("text", json!({"text": "after compaction"}))]),
            message("msg_6", "assistant", 5, vec![part("text", json!({"text": "answer"}))]),
        ],
    })
}

fn outline(drafts: &[Draft]) -> Vec<String> {
    drafts
        .iter()
        .map(|draft| format!("{}: {}", draft.body.kind(), draft.body.text()))
        .collect()
}

#[test]
fn exports_keep_step_order_and_pair_compaction_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let read = bundle_parser::parse_bundle(&exported(), &blobs).unwrap();
    assert_eq!(
        outline(&read.drafts),
        [
            "user: question",
            "thinking: thinking hard",
            r#"tool_call: read {"path":"x"}"#,
            "tool_result: x body",
            "assistant: x looks fine",
            r#"tool_call: bash {"command":"rm"}"#,
            r#"tool_call: bash {"command":"sleep"}"#,
            "tool_result: denied",
            "compaction: compacted",
            "user: after compaction",
            "assistant: answer",
        ]
    );
    assert_eq!(
        read.drafts[7].body,
        Body::ToolResult(ToolResult {
            call_id: "call_b".into(),
            text: "denied".into(),
            images: vec![],
            is_error: true,
        })
    );
    assert_eq!(read.info.title.as_deref(), Some("OpenCode title"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-08-18T12:00:00.000Z")
    );
}

#[test]
fn written_bundles_read_back_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let drafts = bundle_parser::parse_bundle(&exported(), &blobs)
        .unwrap()
        .drafts;
    let events = Event::chain(drafts, None, Agent::Opencode, "ses_x");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Round trip"),
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
    };
    let bundle = bundle_writer::write_bundle(&export, "ses_fixture", "1.19.5", &blobs).unwrap();
    let ids: Vec<&str> = bundle["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["info"]["id"].as_str().unwrap())
        .collect();
    assert!(ids.is_sorted(), "OpenCode orders messages by id");
    let read = bundle_parser::parse_bundle(&bundle, &blobs).unwrap();
    let bodies: Vec<&Body> = read.drafts.iter().map(|draft| &draft.body).collect();
    let expected: Vec<&Body> = replayed.iter().map(|event| &event.body).collect();
    assert_eq!(bodies, expected);
    assert_eq!(read.info.title.as_deref(), Some("Round trip"));
    assert_eq!(read.info.cwd.as_deref(), Some("/work/demo"));

    let untitled = Export {
        title: None,
        ..export
    };
    let bundle = bundle_writer::write_bundle(&untitled, "ses_fixture", "1.19.5", &blobs).unwrap();
    let title = bundle["info"]["title"].as_str().unwrap();
    assert!(
        title.starts_with(UNNAMED),
        "an untitled session gets OpenCode's own placeholder title: {title}"
    );
    let read = bundle_parser::parse_bundle(&bundle, &blobs).unwrap();
    assert_eq!(read.info.title, None, "a placeholder title is no title");
}
