//! Each file based adapter reads native sessions shaped like the ones its agent writes, branches, compactions and titles included.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use baton_adapters::{ClaudeAdapter, CodexAdapter, PiAdapter};
use baton_core::{Adapter, Agent, Blobs, Body, Draft, Image, NativeRead, NativeSession};
use serde_json::{Value, json};

// SHA-256 of the image bytes `pixels`, the name blobs store them under
const PIXELS_SHA256: &str = "6ec9c2b0eb14010746c8bce8939303b382344b296206612eb8a907a37b2b2f37";

fn kinds(drafts: &[Draft]) -> Vec<(String, String)> {
    drafts
        .iter()
        .map(|draft| (draft.body.kind().to_owned(), draft.body.text()))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(kind, text)| ((*kind).to_owned(), (*text).to_owned()))
        .collect()
}

fn only(adapter: &dyn Adapter) -> NativeSession {
    let mut natives = adapter.discover(None).unwrap();
    assert_eq!(natives.len(), 1, "expected exactly one session");
    natives.remove(0)
}

fn read(adapter: &dyn Adapter, blobs: &Blobs) -> NativeRead {
    adapter.read(&only(adapter), None, blobs).unwrap()
}

fn message(id: &str, parent: Option<&str>, second: u32, message: Value) -> Value {
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": format!("2026-01-01T00:00:{second:02}.000Z"), "message": message})
}

#[test]
fn pi_reads_the_active_branch_and_keeps_compacted_entries_after_their_summary() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let records = [
        json!({"type": "session", "version": 3, "id": "pi-session", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/workspace/demo"}),
        message(
            "a1",
            None,
            1,
            json!({"role": "user", "content": "first question", "timestamp": 1}),
        ),
        message(
            "a2",
            Some("a1"),
            2,
            json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "private", "thinkingSignature": "sig"},
            {"type": "text", "text": "let me look"},
            {"type": "toolCall", "id": "call|fc_1", "name": "read", "arguments": {"path": "a.txt"}},
        ], "stopReason": "toolUse"}),
        ),
        message(
            "a3",
            Some("a2"),
            3,
            json!({"role": "toolResult", "toolCallId": "call|fc_1", "toolName": "read", "content": [{"type": "text", "text": "file body"}], "isError": false}),
        ),
        message(
            "b1",
            Some("a1"),
            4,
            json!({"role": "user", "content": "abandoned branch"}),
        ),
        message(
            "a4",
            Some("a3"),
            5,
            json!({"role": "assistant", "content": [{"type": "text", "text": "it says file body"}], "stopReason": "stop"}),
        ),
        json!({"type": "compaction", "id": "c1", "parentId": "a4", "timestamp": "2026-01-01T00:00:06.000Z", "summary": "asked about a.txt", "firstKeptEntryId": "a4", "tokensBefore": 100}),
        json!({"type": "model_change", "id": "m1", "parentId": "c1", "timestamp": "2026-01-01T00:00:07.000Z", "provider": "openai", "modelId": "gpt"}),
        json!({"type": "custom", "id": "x1", "parentId": "m1", "timestamp": "2026-01-01T00:00:07.000Z", "customType": "state", "data": {}}),
        message(
            "a5",
            Some("x1"),
            8,
            json!({"role": "user", "content": [
                {"type": "text", "text": "look at this"},
                {"type": "image", "data": "cGl4ZWxz", "mimeType": "image/png"},
            ]}),
        ),
        message(
            "a6",
            Some("a5"),
            9,
            json!({"role": "assistant", "content": [{"type": "text", "text": "a tiny image"}], "stopReason": "stop"}),
        ),
        json!({"type": "session_info", "id": "s1", "parentId": "a6", "timestamp": "2026-01-01T00:00:10.000Z", "name": "Pi fixture"}),
    ];
    let root = dir.path().join("pi");
    support::write(
        &root.join("sessions/--workspace-demo--/2026-01-01T00-00-00-000Z_pi-session.jsonl"),
        &support::jsonl(&records),
    );
    let adapter = PiAdapter::new(Agent::Pi, root);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "first question"),
            ("thinking", "private"),
            ("assistant", "let me look"),
            ("tool_call", r#"read {"path":"a.txt"}"#),
            ("tool_result", "file body"),
            ("compaction", "asked about a.txt"),
            ("assistant", "it says file body"),
            (
                "meta",
                r#"{"model":"gpt","provider":"openai","type":"model"}"#
            ),
            ("user", "look at this"),
            ("assistant", "a tiny image"),
        ])
    );
    assert_eq!(
        read.drafts[3].body,
        support::call("call_fc_1", "read", json!({"path": "a.txt"}))
    );
    assert_eq!(
        read.drafts[8].body,
        support::user(
            "look at this",
            vec![Image {
                blob: PIXELS_SHA256.into(),
                mime: "image/png".into(),
            }]
        )
    );
    assert_eq!(blobs.get(PIXELS_SHA256).unwrap(), b"pixels");
    assert_eq!(read.info.title.as_deref(), Some("Pi fixture"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/demo"));
    assert_eq!(read.progress.leaf.as_deref(), Some("s1"));
}

fn title_slot(title: &str) -> String {
    let record = |pad: usize| {
        format!(
            "{}\n",
            json!({"type": "title", "v": 1, "title": title, "source": "auto", "updatedAt": "2026-01-01T00:00:00.000Z", "pad": " ".repeat(pad)})
        )
    };
    record(256 - record(0).len())
}

#[test]
fn omp_reads_title_slots_blob_images_and_reset_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("omp");
    // SHA-256 of `webp bytes`, which Oh My Pi names the blob file after
    let digest = "ac25bbaf1a9cd1550a99f74863c51cfdb6fb4d3947a3f8e5f03a4bae8c50e2d4";
    support::write(&root.join("blobs").join(digest), "webp bytes");
    let records = [
        json!({"type": "session", "version": 3, "id": "omp-session", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/workspace/omp"}),
        json!({"type": "model_change", "id": "m1", "parentId": null, "timestamp": "2026-01-01T00:00:00.000Z", "model": "anthropic/claude"}),
        message(
            "u1",
            Some("m1"),
            1,
            json!({"role": "user", "attribution": "user", "content": [
                {"type": "text", "text": "before reset"},
                {"type": "image", "mimeType": "image/webp", "data": format!("blob:sha256:{digest}")},
            ]}),
        ),
        json!({"type": "title_change", "id": "t1", "parentId": "u1", "timestamp": "2026-01-01T00:00:02.000Z", "title": "Old title", "source": "auto"}),
        json!({"type": "reset_boundary", "id": "r1", "parentId": "t1", "timestamp": "2026-01-01T00:00:03.000Z"}),
        message(
            "u2",
            Some("r1"),
            4,
            json!({"role": "user", "content": "after reset"}),
        ),
        message(
            "a2",
            Some("u2"),
            5,
            json!({"role": "assistant", "content": [{"type": "text", "text": "fresh start"}], "stopReason": "stop"}),
        ),
    ];
    let text = title_slot("Slot title") + &support::jsonl(&records);
    support::write(
        &root.join("sessions/-workspace-omp/2026-01-01T00-00-00-000Z_omp-session.jsonl"),
        &text,
    );
    let adapter = PiAdapter::new(Agent::Omp, root.clone());
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("meta", r#"{"model":"anthropic/claude","type":"model"}"#),
            ("user", "before reset"),
            ("meta", r#"{"type":"reset"}"#),
            ("user", "after reset"),
            ("assistant", "fresh start"),
        ])
    );
    let image = match &read.drafts[1].body {
        Body::User(user) => user.images[0].clone(),
        other => panic!("expected the user message, got {other:?}"),
    };
    assert_eq!(image.blob, digest);
    assert_eq!(read.info.title.as_deref(), Some("Slot title"));
}

#[test]
fn claude_reads_the_main_chain_across_compaction_and_split_messages() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let session = "21000000-0000-4000-8000-000000000001";
    let base = |kind: &str, uuid: &str, parent: Option<&str>, second: u32| {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": session, "cwd": "/workspace/drift",
               "isSidechain": false, "timestamp": format!("2026-01-06T10:00:{second:02}.000Z")})
    };
    let with = |mut record: Value, fields: Value| {
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        record
    };
    let records = [
        json!({"type": "permission-mode", "permissionMode": "default", "sessionId": session}),
        with(
            base("user", "u1", None, 1),
            json!({"message": {"role": "user", "content": "Synthetic request"}}),
        ),
        with(
            base("assistant", "a1", Some("u1"), 2),
            json!({"message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hidden", "signature": "sig"},
                {"type": "text", "text": "answer part"},
                {"type": "tool_use", "id": "toolu_1", "name": "Task", "input": {"prompt": "inspect"}},
            ]}}),
        ),
        with(
            base("user", "r1", Some("a1"), 3),
            json!({"message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "report"}]},
        ]}, "toolUseResult": {"status": "completed"}}),
        ),
        with(
            base("user", "side", Some("r1"), 3),
            json!({"isSidechain": true, "message": {"role": "user", "content": "subagent only"}}),
        ),
        json!({"type": "ai-title", "aiTitle": "AI title", "sessionId": session}),
        with(
            base("assistant", "a2a", Some("r1"), 4),
            json!({"message": {"id": "msg_2", "role": "assistant", "content": [{"type": "text", "text": "part one"}]}}),
        ),
        with(
            base("assistant", "a2b", Some("a2a"), 4),
            json!({"message": {"id": "msg_2", "role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {"command": "false"}},
            ]}}),
        ),
        with(
            base("user", "r2", Some("a2b"), 5),
            json!({"message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "exit 1", "is_error": true},
            ]}}),
        ),
        with(
            base("assistant", "syn", Some("r2"), 5),
            json!({"message": {"role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": "No response requested."}]}}),
        ),
        with(
            base("system", "cb", None, 6),
            json!({"subtype": "compact_boundary", "logicalParentUuid": "syn", "content": "Conversation compacted"}),
        ),
        with(
            base("user", "cs", Some("cb"), 6),
            json!({"isCompactSummary": true, "message": {"role": "user", "content": "summary text"}}),
        ),
        with(
            base("user", "u2", Some("cs"), 7),
            json!({"message": {"role": "user", "content": [
                {"type": "text", "text": "with image"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "cGl4ZWxz"}},
            ]}}),
        ),
        with(
            base("user", "meta", Some("u2"), 8),
            json!({"isMeta": true, "message": {"role": "user", "content": "caveat"}}),
        ),
        with(
            base("assistant", "a3", Some("meta"), 9),
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "final"}]}}),
        ),
        json!({"type": "custom-title", "customTitle": "Custom title", "sessionId": session}),
        json!({"type": "last-prompt", "lastPrompt": "Synthetic request", "leafUuid": "u2", "sessionId": session}),
    ];
    let root = dir.path().join("claude");
    support::write(
        &root.join(format!("projects/-workspace-drift/{session}.jsonl")),
        &support::jsonl(&records),
    );
    support::write(
        &root.join("projects/-workspace-drift/agent-a0000001.jsonl"),
        "{}\n",
    );
    let adapter = ClaudeAdapter::new(root);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "Synthetic request"),
            ("thinking", "hidden"),
            ("assistant", "answer part"),
            ("tool_call", r#"Task {"prompt":"inspect"}"#),
            ("tool_result", "report"),
            ("assistant", "part one"),
            ("tool_call", r#"Bash {"command":"false"}"#),
            ("tool_result", "exit 1"),
            ("compaction", "summary text"),
            ("user", "with image"),
            ("assistant", "final"),
        ])
    );
    assert_eq!(
        read.drafts[7].body,
        support::result("toolu_2", "exit 1", vec![], true)
    );
    assert_eq!(read.info.title.as_deref(), Some("Custom title"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/drift"));
}

fn rollout(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    let path = root.join("sessions/2026/01/05").join(name);
    support::write(&path, &support::jsonl(records));
    path
}

#[test]
fn codex_legacy_rollouts_follow_rollbacks_and_index_titles() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("codex");
    let id = "11111111-1111-4111-8111-111111111111";
    let at = |second: u32| format!("2026-01-05T09:00:{second:02}.000Z");
    let record = |second: u32, kind: &str, payload: Value| json!({"timestamp": at(second), "type": kind, "payload": payload});
    let user_turn = |second: u32, text: &str| {
        [
            record(
                second,
                "event_msg",
                json!({"type": "user_message", "message": text, "images": []}),
            ),
            record(
                second,
                "response_item",
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
            ),
        ]
    };
    let answer = |second: u32, text: &str| {
        [
            record(
                second,
                "event_msg",
                json!({"type": "agent_message", "message": text}),
            ),
            record(
                second,
                "response_item",
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}),
            ),
        ]
    };
    let mut records = vec![
        record(
            0,
            "session_meta",
            json!({"id": id, "timestamp": at(0), "cwd": "/workspace/codex", "source": "cli"}),
        ),
        record(
            1,
            "turn_context",
            json!({"cwd": "/workspace/codex", "model": "gpt"}),
        ),
        record(
            1,
            "response_item",
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>env</environment_context>"}]}),
        ),
    ];
    records.extend(user_turn(2, "First request"));
    records.push(record(
        3,
        "response_item",
        json!({"type": "reasoning", "summary": [], "encrypted_content": "opaque"}),
    ));
    records.push(record(4, "response_item", json!({"type": "function_call", "name": "shell", "arguments": "{\"command\":[\"ls\"]}", "call_id": "call-1"})));
    records.push(record(
        5,
        "response_item",
        json!({"type": "function_call_output", "call_id": "call-1", "output": "listing"}),
    ));
    records.extend(answer(6, "First answer"));
    records.push(record(
        7,
        "compacted",
        json!({"message": "compacted summary"}),
    ));
    records.extend(user_turn(8, "Discarded request"));
    records.extend(answer(9, "Discarded answer"));
    records.push(record(
        10,
        "event_msg",
        json!({"type": "thread_rolled_back", "num_turns": 1}),
    ));
    records.extend(user_turn(11, "Replacement request"));
    records.extend(answer(12, "Replacement answer"));
    rollout(
        &root,
        &format!("rollout-2026-01-05T09-00-00-{id}.jsonl"),
        &records,
    );
    rollout(
        &root,
        "rollout-2026-01-05T09-05-00-33333333-3333-4333-8333-333333333333.jsonl",
        &[record(
            0,
            "session_meta",
            json!({"id": "33333333-3333-4333-8333-333333333333", "cwd": "/w", "thread_source": "subagent", "source": {"subagent": {}}}),
        )],
    );
    support::write(
        &root.join("session_index.jsonl"),
        &support::jsonl(&[
            json!({"id": id, "thread_name": "Old name", "updated_at": at(5)}),
            json!({"id": id, "thread_name": "Indexed name", "updated_at": at(9)}),
        ]),
    );
    let adapter = CodexAdapter::new(root);
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Indexed name"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "First request"),
            ("thinking", ""),
            ("tool_call", r#"shell {"command":["ls"]}"#),
            ("tool_result", "listing"),
            ("assistant", "First answer"),
            ("compaction", "compacted summary"),
            ("user", "Replacement request"),
            ("assistant", "Replacement answer"),
        ])
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/codex"));
}

#[test]
fn codex_paginated_rollouts_read_completed_items_from_compressed_files() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("codex");
    let target = root.join("sessions/2026/08/24/rollout-2026-08-24T12-53-08-01a0321d-7ed8-76e2-aaa1-6a987e5d44b0.jsonl.zst");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-paginated.jsonl.zst"),
        &target,
    )
    .unwrap();
    let adapter = CodexAdapter::new(root);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "Hello paginated"),
            ("tool_call", r#"exec {"cmd":"pwd"}"#),
            ("tool_result", "/w"),
            ("assistant", "Hi from agent"),
        ])
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/paginated"));
}
