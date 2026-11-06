// The bundle conversions are tested directly because the adapter itself needs a real `opencode` command

use std::path::Path;

use baton_core::{
    Agent, Assistant, Blobs, Body, Draft, Event, Export, Image, ToolCall, ToolResult, User, replay,
};
use serde_json::{Value, json};

use crate::opencode::bundle_writer::{self, UNNAMED};
use crate::opencode::{bundle_parser, transfer_parser, transfer_writer};

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

fn ordering_drafts(blobs: &Blobs) -> Vec<Draft> {
    let call = |id: &str, name: &str| {
        Body::ToolCall(ToolCall {
            call_id: id.into(),
            name: name.into(),
            input: json!({"id": id}),
        })
    };
    let result = |id: &str, text: &str, is_error: bool, images: Vec<Image>| {
        Body::ToolResult(ToolResult {
            call_id: id.into(),
            text: text.into(),
            images,
            is_error,
        })
    };
    let image = Image {
        blob: blobs.put(b"result image").unwrap(),
        mime: "image/png".into(),
    };
    [
        call("direct", "read"),
        result("direct", "immediate", false, vec![]),
        call("parallel_a", "read"),
        call("parallel_b", "bash"),
        result("parallel_a", "first", false, vec![]),
        result("parallel_b", "second", false, vec![]),
        call("with_text", "read"),
        Body::Assistant(Assistant {
            text: "after the call".into(),
        }),
        result("with_text", "after text", false, vec![]),
        call("late_a", "read"),
        call("late_b", "bash"),
        Body::Assistant(Assistant {
            text: "between calls and results".into(),
        }),
        result("late_b", "failed", true, vec![image.clone()]),
        result("late_a", "finished", false, vec![image]),
        call("delayed", "read"),
        Body::User(User {
            text: "intervening user".into(),
            images: vec![],
        }),
        result("delayed", "delayed result", false, vec![]),
        result("missing", "orphan", false, vec![]),
    ]
    .into_iter()
    .map(|body| Draft {
        timestamp: "2026-08-18T12:00:10.000Z".into(),
        native_entry_id: None,
        body,
    })
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
    let mut drafts = bundle_parser::parse_bundle(&exported(), &blobs)
        .unwrap()
        .drafts;
    drafts.extend(ordering_drafts(&blobs));
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
    let parts: Vec<&Value> = bundle["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .collect();
    let direct = parts
        .iter()
        .find(|part| part["callID"] == "direct")
        .unwrap();
    assert_eq!(direct["state"]["output"], "immediate");
    for (id, output) in [
        ("parallel_a", "first"),
        ("parallel_b", "second"),
        ("with_text", "after text"),
    ] {
        let part = parts.iter().find(|part| part["callID"] == id).unwrap();
        assert_eq!(part["state"]["output"], output);
    }
    let late = parts
        .iter()
        .find(|part| {
            part["state"]["input"] == json!({"id": "late_b"}) && part["state"]["error"] == "failed"
        })
        .unwrap();
    assert_eq!(late["tool"], "bash");
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

// What `opencode session export` prints on OpenCode 2 for a session with an image, a failed and an unfinished tool, a model switch and a compaction
fn transferred() -> Value {
    let at = |second: i64| json!({"created": 1_787_054_400_000_i64 + second * 1000, "completed": 1_787_054_400_000_i64 + second * 1000});
    json!({
        "info": {"id": "ses_y", "projectID": "global", "title": "OpenCode 2 title", "location": {"directory": "/workspace/oc2"},
                 "cost": 0, "time": {"created": 1_787_054_400_000_i64, "updated": 1_787_054_406_000_i64}},
        "messages": [
            {"id": "msg_1", "type": "user", "time": at(0), "text": "question", "files": [
                {"data": "cGl4ZWxz", "mime": "image/png", "source": {"type": "inline"}},
                {"data": "eA==", "mime": "text/plain", "source": {"type": "inline"}},
            ]},
            {"id": "msg_2", "type": "assistant", "time": at(1), "content": [
                {"type": "reasoning", "text": "thinking hard"},
                {"type": "text", "text": "reading"},
                {"type": "tool", "id": "call_a", "name": "read", "state": {"status": "completed", "input": {"path": "x"},
                    "content": [{"type": "text", "text": "x body"}, {"type": "file", "uri": "data:image/png;base64,cGl4ZWxz", "mime": "image/png"}]}},
                {"type": "tool", "id": "call_b", "name": "bash", "state": {"status": "error", "input": {"command": "rm"}, "error": {"type": "ToolError", "message": "denied"}}},
                {"type": "tool", "id": "call_c", "name": "bash", "state": {"status": "running", "input": {"command": "sleep"}, "metadata": {}}},
            ]},
            {"id": "msg_3", "type": "model-switched", "time": at(2), "model": {"id": "gpt", "providerID": "openai"}},
            {"id": "msg_4", "type": "compaction", "time": at(3), "status": "completed", "reason": "auto", "summary": "compacted"},
            {"id": "msg_5", "type": "synthetic", "time": at(4), "text": "injected context"},
            {"id": "msg_6", "type": "user", "time": at(5), "text": "after compaction"},
            {"id": "msg_7", "type": "assistant", "time": at(6), "content": [{"type": "text", "text": "answer"}]},
        ],
    })
}

#[test]
fn v2_exports_list_results_after_their_message() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let read = transfer_parser::parse_transfer(&transferred(), &blobs).unwrap();
    assert_eq!(
        outline(&read.drafts),
        [
            "user: question",
            "thinking: thinking hard",
            "assistant: reading",
            r#"tool_call: read {"path":"x"}"#,
            r#"tool_call: bash {"command":"rm"}"#,
            r#"tool_call: bash {"command":"sleep"}"#,
            "tool_result: x body",
            "tool_result: denied",
            r#"meta: {"model":"openai/gpt","type":"model"}"#,
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
    assert_eq!(read.info.title.as_deref(), Some("OpenCode 2 title"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/oc2"));
}

#[test]
fn written_v2_transfers_read_back_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let mut drafts = transfer_parser::parse_transfer(&transferred(), &blobs)
        .unwrap()
        .drafts;
    drafts.extend(ordering_drafts(&blobs));
    let events = Event::chain(drafts, None, Agent::Opencode, "ses_y");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: None,
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
    };
    let data = transfer_writer::write_transfer(&export, "ses_fixture", &blobs).unwrap();
    let blocks: Vec<&Value> = data["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .collect();
    let direct = blocks.iter().find(|block| block["id"] == "direct").unwrap();
    assert_eq!(direct["state"]["content"][0]["text"], "immediate");
    for (id, output) in [
        ("parallel_a", "first"),
        ("parallel_b", "second"),
        ("with_text", "after text"),
    ] {
        let block = blocks.iter().find(|block| block["id"] == id).unwrap();
        assert_eq!(block["state"]["content"][0]["text"], output);
    }
    let late = blocks
        .iter()
        .find(|block| {
            block["state"]["input"] == json!({"id": "late_b"})
                && block["state"]["error"]["message"] == "failed"
        })
        .unwrap();
    assert_eq!(late["name"], "bash");
    let ids: Vec<&str> = data["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["id"].as_str().unwrap())
        .collect();
    assert!(ids.is_sorted(), "OpenCode orders messages by id");
    let read = transfer_parser::parse_transfer(&data, &blobs).unwrap();
    let bodies: Vec<&Body> = read.drafts.iter().map(|draft| &draft.body).collect();
    let expected: Vec<&Body> = replayed.iter().map(|event| &event.body).collect();
    assert_eq!(bodies, expected);
    assert_eq!(read.info.title, None, "OpenCode 2 has no placeholder title");
    assert_eq!(read.info.cwd.as_deref(), Some("/work/demo"));
}
