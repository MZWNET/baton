//! Each file based adapter reads native sessions shaped like the ones its agent writes, branches, compactions and titles included.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use baton_adapters::{
    ClaudeAdapter, CodexAdapter, CopilotAdapter, DevinAdapter, DshAdapter, GrokAdapter,
    HermesAdapter, KimiAdapter, MastracodeAdapter, MuseAdapter, OpenhandsAdapter, Paseo, PiAdapter,
    QwenAdapter, VibeAdapter,
};
use baton_core::{Agent, Blobs, Body, Draft, Image, NativeRead, NativeSession, Source};
use serde_json::{Value, json};

// SHA-256 of the image bytes `pixels`, the name blobs store them under
const PIXELS_SHA256: &str = "6ec9c2b0eb14010746c8bce8939303b382344b296206612eb8a907a37b2b2f37";

#[test]
fn paseo_reads_supported_active_records_and_path_handles() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("agents/project");
    fs::create_dir_all(&directory).unwrap();
    for (id, provider, handle, internal, archived) in [
        ("claude", "claude", "native-id", false, false),
        ("pi", "pi", "/home/me/session.jsonl", false, false),
        ("internal", "claude", "internal-id", true, false),
        ("archived", "claude", "archived-id", false, true),
        ("unsupported", "copilot", "other-id", false, false),
    ] {
        let record = json!({"id": id, "provider": provider, "cwd": "/work", "title": "name", "internal": internal, "archivedAt": if archived { Some("2026-01-01") } else { None }, "persistence": {"provider": provider, "sessionId": handle, "nativeHandle": handle, "metadata": {"mcpServers": {"secret": "never copied"}}}});
        fs::write(directory.join(format!("{id}.json")), record.to_string()).unwrap();
    }
    let agents = Paseo::new(home.path().to_path_buf()).discover().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(
        agents
            .iter()
            .any(|agent| agent.provider == Agent::Claude && agent.handle == "native-id")
    );
    assert!(
        agents
            .iter()
            .any(|agent| agent.provider == Agent::Pi && agent.handle == "/home/me/session.jsonl")
    );
}

fn kinds(drafts: &[Draft]) -> Vec<(String, String)> {
    drafts
        .iter()
        .map(|draft| (draft.body.kind().to_owned(), draft.body.text().into_owned()))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(kind, text)| ((*kind).to_owned(), (*text).to_owned()))
        .collect()
}

fn only(adapter: &dyn Source) -> NativeSession {
    let mut natives = adapter.discover(None).unwrap();
    assert_eq!(natives.len(), 1, "expected exactly one session");
    natives.remove(0)
}

fn read(adapter: &dyn Source, blobs: &Blobs) -> NativeRead {
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

fn pixels() -> Image {
    Image {
        blob: PIXELS_SHA256.into(),
        mime: "image/png".into(),
    }
}

#[test]
fn hermes_reads_active_rows_across_a_rotated_compression() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let home = dir.path().join("hermes");
    let connection = support::database(
        &home.join("state.db"),
        include_str!("fixtures/hermes-state.sql"),
    );
    connection
        .execute_batch(
            r#"
INSERT INTO sessions (id, source, started_at, ended_at, end_reason, cwd, title) VALUES
  ('20260101_090000_aaaaaa', 'cli', 1767258000.0, 1767258060.0, 'compression', '/workspace/hermes', 'Hermes fixture');
INSERT INTO sessions (id, source, parent_session_id, started_at, cwd) VALUES
  ('20260101_090100_bbbbbb', 'cli', '20260101_090000_aaaaaa', 1767258060.0, '/workspace/hermes');
INSERT INTO sessions (id, source, started_at) VALUES ('20260101_090200_cccccc', 'tool', 1767258120.0);
INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, reasoning, timestamp, active, _compressed_summary) VALUES
  ('20260101_090000_aaaaaa', 'user', 'first question', NULL, NULL, NULL, 1767258001.0, 1, 0),
  ('20260101_090000_aaaaaa', 'assistant', '', '[{"id": "call_1|fc_1", "type": "function", "function": {"name": "terminal", "arguments": "{\"command\": \"ls\"}"}}]', NULL, NULL, 1767258002.0, 1, 0),
  ('20260101_090000_aaaaaa', 'tool', '{"output": "", "exit_code": 1, "error": null}', NULL, 'call_1|fc_1', NULL, 1767258003.0, 1, 0),
  ('20260101_090000_aaaaaa', 'user', 'taken back by a rewind', NULL, NULL, NULL, 1767258004.0, 0, 0),
  ('20260101_090000_aaaaaa', 'assistant', 'it failed', NULL, NULL, 'private', 1767258005.0, 1, 0),
  ('20260101_090100_bbbbbb', 'user', '[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below.' || char(10) || 'summary body' || char(10) || char(10) || '--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---' || char(10) || char(10) || 'live question', NULL, NULL, NULL, 1767258061.0, 1, 1),
  ('20260101_090100_bbbbbb', 'user', char(0) || 'json:[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,cGl4ZWxz"}}]', NULL, NULL, NULL, 1767258062.0, 1, 0),
  ('20260101_090100_bbbbbb', 'assistant', 'final', NULL, NULL, NULL, 1767258063.0, 1, 0);
"#,
        )
        .unwrap();
    let adapter = HermesAdapter::new(home);
    let native = only(&adapter);
    assert_eq!(
        native.id, "20260101_090000_aaaaaa",
        "the continuation of a rotated session and tool sessions are not listed"
    );
    assert_eq!(native.title.as_deref(), Some("Hermes fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    let envelope = r#"{"output": "", "exit_code": 1, "error": null}"#;
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "first question"),
            ("tool_call", r#"terminal {"command":"ls"}"#),
            ("tool_result", envelope),
            ("thinking", "private"),
            ("assistant", "it failed"),
            ("compaction", "summary body"),
            ("user", "live question"),
            ("user", "look"),
            ("assistant", "final"),
        ])
    );
    assert_eq!(
        read.drafts[2].body,
        support::result("call_1_fc_1", envelope, vec![], true)
    );
    assert_eq!(read.drafts[7].body, support::user("look", vec![pixels()]));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/hermes"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T09:00:00.000Z")
    );
}

#[test]
fn mastracode_reads_user_signals_and_results_folded_into_their_steps() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("mastra.db");
    let connection = support::database(
        &db,
        r#"
CREATE TABLE mastra_threads ("id" TEXT NOT NULL PRIMARY KEY, "resourceId" TEXT NOT NULL, "title" TEXT NOT NULL,
  "metadata" TEXT, "createdAt" TEXT NOT NULL, "updatedAt" TEXT NOT NULL);
CREATE TABLE mastra_messages ("id" TEXT NOT NULL PRIMARY KEY, "thread_id" TEXT NOT NULL, "content" TEXT NOT NULL,
  "role" TEXT NOT NULL, "type" TEXT NOT NULL, "createdAt" TEXT NOT NULL, "resourceId" TEXT);
"#,
    );
    connection
        .execute_batch(
            r#"
INSERT INTO mastra_threads VALUES
  ('t1', 'work-1', 'Mastra fixture', jsonb('{"projectPath":"/workspace/mastra"}'), '2026-01-01T09:00:00.000Z', '2026-01-01T09:05:00.000Z'),
  ('t2', 'work-1', '', jsonb('{"forkedSubagent":true}'), '2026-01-01T09:00:00.000Z', '2026-01-01T09:05:00.000Z'),
  ('t3', 'work-1', 'empty', NULL, '2026-01-01T09:00:00.000Z', '2026-01-01T09:05:00.000Z');
INSERT INTO mastra_messages VALUES
  ('m1', 't1', '{"format":2,"metadata":{"signal":{"type":"user"}},"parts":[{"type":"text","text":"inspect this"},{"type":"file","data":"cGl4ZWxz","mimeType":"image/png"}]}', 'signal', 'user', '2026-01-01T09:00:01.000Z', 'work-1'),
  ('__temporal_gap', 't1', '{"format":2,"metadata":{"signal":{"type":"reactive"}},"parts":[{"type":"text","text":"time passed"}]}', 'signal', 'reactive', '2026-01-01T09:00:02.000Z', 'work-1'),
  ('m2', 't1', '{"format":2,"parts":[{"type":"tool-invocation","toolInvocation":{"state":"result","toolCallId":"call_a","toolName":"execute_command","args":{"command":"ls"},"result":"raw output"},"providerMetadata":{"mastra":{"modelOutput":{"type":"text","value":"model saw this"}}}},{"type":"data-sandbox-exit","data":{"exitCode":0}},{"type":"step-start"},{"type":"reasoning","reasoning":"hmm","providerMetadata":{"anthropic":{"signature":"sig"}}},{"type":"text","text":"answer one"},{"type":"tool-invocation","toolInvocation":{"state":"output-error","toolCallId":"call_b","toolName":"view","args":{"path":"x"},"errorText":"File not found: x"}}]}', 'assistant', 'v2', '2026-01-01T09:00:03.000Z', 'work-1'),
  ('om-continuation', 't1', '{"format":2,"parts":[{"type":"text","text":"continue"}]}', 'user', 'v2', '2026-01-01T09:00:04.000Z', 'work-1'),
  ('m3', 't1', '{"format":2,"metadata":{"signal":{"type":"notification"}},"parts":[{"type":"text","text":"build finished"}]}', 'signal', 'notification', '2026-01-01T09:00:05.000Z', 'work-1'),
  ('m4', 't1', '{"format":2,"parts":[{"type":"text","text":"second"}]}', 'user', 'v2', '2026-01-01T09:00:06.000Z', 'work-1'),
  ('m5', 't2', '{"format":2,"parts":[{"type":"text","text":"subagent"}]}', 'user', 'v2', '2026-01-01T09:00:06.000Z', 'work-1');
"#,
        )
        .unwrap();
    let adapter = MastracodeAdapter::new(db);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "inspect this"),
            ("tool_call", r#"execute_command {"command":"ls"}"#),
            ("tool_result", "model saw this"),
            ("thinking", "hmm"),
            ("assistant", "answer one"),
            ("tool_call", r#"view {"path":"x"}"#),
            ("tool_result", "File not found: x"),
            ("user", "second"),
        ])
    );
    assert_eq!(
        read.drafts[0].body,
        support::user("inspect this", vec![pixels()])
    );
    assert_eq!(
        read.drafts[6].body,
        support::result("call_b", "File not found: x", vec![], true)
    );
    assert_eq!(read.info.title.as_deref(), Some("Mastra fixture"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/mastra"));
}

#[test]
fn devin_reads_the_main_chain_without_injected_context() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("devin/sessions.db");
    let connection = support::database(&db, include_str!("fixtures/devin-sessions.sql"));
    connection
        .execute_batch(
            r#"
INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id, hidden) VALUES
  ('brisk-otter', '/workspace/devin', 'windsurf', 'swe-1-6-slow', 'normal', 1767258000, 1767258100, 'Devin fixture', 6, 0),
  ('summarizer', '/workspace/devin', 'windsurf', 'swe-1-6-slow', 'normal', 1767258000, 1767258100, NULL, 0, 1),
  ('fresh', '/workspace/devin', 'windsurf', 'swe-1-6-slow', 'normal', 1767258000, 1767258100, NULL, NULL, 0);
INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) VALUES
  ('brisk-otter', 0, NULL, '{"message_id":"m0","role":"system","content":"system prompt"}', 1767258000),
  ('brisk-otter', 1, 0, '{"message_id":"m1","role":"user","content":"look","images":[{"width":1,"height":1,"base64_data":"cGl4ZWxz","mime_type":"image/png"}],"metadata":{"is_user_input":true,"created_at":"2026-01-01T09:00:01.123456789Z"}}', 1767258001),
  ('brisk-otter', 2, 1, '{"message_id":"m2","role":"assistant","content":"reading","thinking":{"thinking":"hmm","signature":"sig"},"tool_calls":[{"id":"call_a","name":"read","arguments":{"file_path":"a"},"index":0,"kind":"function"},{"id":"call_b","name":"read","arguments":{"file_path":"b"},"index":1,"kind":"function"}],"metadata":{}}', 1767258002),
  ('brisk-otter', 3, 2, '{"message_id":"m3","role":"tool","content":"a body","tool_call_id":"call_a","metadata":{}}', 1767258003),
  ('brisk-otter', 4, 3, '{"message_id":"m4","role":"tool","content":"Failed to read file","tool_call_id":"call_b","metadata":{"extensions":{"chisel/tool_result_meta":{"success":false}}}}', 1767258004),
  ('brisk-otter', 5, 4, '{"message_id":"m5","role":"user","content":"workspace context","metadata":{"is_user_input":null}}', 1767258005),
  ('brisk-otter', 6, 5, '{"message_id":"m6","role":"assistant","content":"done","tool_calls":[],"metadata":{}}', 1767258006),
  ('brisk-otter', 7, 1, '{"message_id":"m7","role":"assistant","content":"abandoned retry","metadata":{}}', 1767258007);
"#,
        )
        .unwrap();
    let adapter = DevinAdapter::new(db);
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Devin fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "look"),
            ("thinking", "hmm"),
            ("assistant", "reading"),
            ("tool_call", r#"read {"file_path":"a"}"#),
            ("tool_call", r#"read {"file_path":"b"}"#),
            ("tool_result", "a body"),
            ("tool_result", "Failed to read file"),
            ("assistant", "done"),
        ])
    );
    assert_eq!(read.drafts[0].body, support::user("look", vec![pixels()]));
    assert_eq!(read.drafts[0].timestamp, "2026-01-01T09:00:01.123Z");
    assert_eq!(
        read.drafts[6].body,
        support::result("call_b", "Failed to read file", vec![], true)
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/devin"));
}

#[test]
fn copilot_reads_its_log_in_line_order_and_keeps_messages_sent_during_a_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("copilot");
    let id = "89898989-8989-4989-8989-898989898989";
    let mut second = 0;
    let mut record = |kind: &str, data: Value| {
        second += 1;
        json!({"type": kind, "data": data, "id": format!("e{second}"), "timestamp": format!("2026-01-01T00:00:{second:02}.000Z"), "parentId": null})
    };
    let mut records = vec![
        record(
            "session.start",
            json!({"sessionId": id, "version": 1, "producer": "copilot-agent", "copilotVersion": "1.0.70", "startTime": "2026-01-01T00:00:00.500Z", "context": {"cwd": "/workspace/copilot"}}),
        ),
        record("session.model_change", json!({"newModel": "gpt-4.1"})),
        record(
            "system.message",
            json!({"role": "system", "content": "system prompt"}),
        ),
        record(
            "session.binary_asset",
            json!({"assetId": format!("sha256:{PIXELS_SHA256}"), "type": "image", "mimeType": "image/png", "byteLength": 6, "data": "cGl4ZWxz"}),
        ),
        record(
            "user.message",
            json!({"content": "look", "transformedContent": "<current_datetime>now</current_datetime> look", "attachments": [
                {"type": "file", "path": "/workspace/copilot/card.png", "assetId": format!("sha256:{PIXELS_SHA256}"), "mimeType": "image/png"},
                {"type": "file", "path": "/workspace/copilot/doc.pdf", "displayName": "doc.pdf"},
            ]}),
        ),
        record(
            "assistant.message",
            json!({"messageId": "m1", "content": "", "toolRequests": [{"toolCallId": "call_view", "name": "view", "arguments": {"path": "a.txt"}, "type": "function"}]}),
        ),
        record(
            "tool.execution_start",
            json!({"toolCallId": "call_view", "toolName": "view", "arguments": {"path": "a.txt"}}),
        ),
        record(
            "tool.execution_complete",
            json!({"toolCallId": "call_view", "success": false, "error": {"message": "missing"}, "result": {"content": "a.txt is missing", "detailedContent": "diff"}}),
        ),
        record(
            "user.message",
            json!({"content": "reminder", "source": "system"}),
        ),
        record(
            "assistant.message",
            json!({"messageId": "m2", "content": "it is missing", "reasoningText": "hmm", "reasoningOpaque": "opaque"}),
        ),
        record("session.compaction_start", json!({})),
        record(
            "user.message",
            json!({"content": "sent during the compaction"}),
        ),
        record(
            "session.compaction_complete",
            json!({"success": true, "summaryContent": "looked for a.txt"}),
        ),
    ];
    let mut subagent = record(
        "assistant.message",
        json!({"messageId": "m3", "content": "subagent work"}),
    );
    subagent["agentId"] = json!("agent-1");
    records.push(subagent);
    records.push(record(
        "assistant.message",
        json!({"messageId": "m4", "content": "done"}),
    ));
    let session = root.join("session-state").join(id);
    support::write(&session.join("events.jsonl"), &support::jsonl(&records));
    support::write(
        &session.join("workspace.yaml"),
        "id: x\nname: Copilot fixture\n",
    );
    support::write(
        &root.join("session-state/not-a-session/events.jsonl"),
        "{}\n",
    );
    let adapter = CopilotAdapter::new(root);
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Copilot fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("meta", r#"{"model":"gpt-4.1","type":"model"}"#),
            ("user", "look"),
            ("tool_call", r#"view {"path":"a.txt"}"#),
            ("tool_result", "a.txt is missing"),
            ("thinking", "hmm"),
            ("assistant", "it is missing"),
            ("compaction", "looked for a.txt"),
            ("user", "sent during the compaction"),
            ("assistant", "done"),
        ])
    );
    assert_eq!(read.drafts[1].body, support::user("look", vec![pixels()]));
    assert_eq!(
        read.drafts[3].body,
        support::result("call_view", "a.txt is missing", vec![], true)
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/copilot"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T00:00:00.500Z")
    );
}

#[test]
fn grok_merges_streamed_chunks_and_follows_rewinds() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("grok");
    let id = "95959595-9595-4959-8959-959595959595";
    let mut second: i64 = 0;
    let mut line = |method: &str, update: Value| {
        second += 1;
        json!({"timestamp": 1_767_258_000 + second, "method": method,
               "params": {"sessionId": id, "update": update, "_meta": {"agentTimestampMs": (1_767_258_000 + second) * 1000}}})
    };
    let acp = "session/update";
    let xai = "_x.ai/session/update";
    let records = [
        line(
            acp,
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "look"}, "_meta": {"promptIndex": 0}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "image", "data": "cGl4ZWxz", "mimeType": "image/png"}, "_meta": {"promptIndex": 0}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Reading "}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "the note."}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "tool_call", "toolCallId": "call_read", "title": "read_file", "rawInput": {"target_file": "NOTE.txt"},
                         "_meta": {"x.ai/tool": {"name": "read_file", "kind": "read"}}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_read", "title": "Read `NOTE.txt`", "rawInput": {"variant": "ReadFile"}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_read", "status": "failed",
                         "content": [{"type": "content", "content": {"type": "text", "text": "no such file"}}]}),
        ),
        line(
            xai,
            json!({"sessionUpdate": "turn_completed", "prompt_id": "p1", "stop_reason": "end_turn"}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "/model"}, "_meta": {"hostTurn": true}}),
        ),
        line(
            xai,
            json!({"sessionUpdate": "model_changed", "model_id": "grok-4"}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "abandoned question"}, "_meta": {"promptIndex": 1}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "abandoned answer"}}),
        ),
        line(
            xai,
            json!({"sessionUpdate": "rewind_marker", "target_prompt_index": 1}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "second question"}, "_meta": {"promptIndex": 1}}),
        ),
        line(
            acp,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "second answer"}}),
        ),
    ];
    let session = root.join("sessions/%2Fworkspace%2Fgrok").join(id);
    support::write(&session.join("updates.jsonl"), &support::jsonl(&records));
    support::write(
        &session.join("summary.json"),
        &json!({"info": {"id": id, "cwd": "/workspace/grok"}, "session_summary": "summary text", "generated_title": " Grok fixture ",
                "created_at": "2026-01-01T09:00:00.810221602Z", "updated_at": "2026-01-01T09:01:00Z", "num_messages": 16,
                "current_model_id": "grok-4", "chat_format_version": 1}).to_string(),
    );
    let subagent = root.join("sessions/%2Fworkspace%2Fgrok/96969696-9696-4969-8969-969696969696");
    support::write(&subagent.join("updates.jsonl"), "");
    support::write(&subagent.join("summary.json"), &json!({"info": {"id": "96969696-9696-4969-8969-969696969696", "cwd": "/w"}, "session_kind": "subagent"}).to_string());
    let adapter = GrokAdapter::new(root);
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Grok fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "look"),
            ("thinking", "hmm"),
            ("assistant", "Reading the note."),
            ("tool_call", r#"read_file {"target_file":"NOTE.txt"}"#),
            ("tool_result", "no such file"),
            ("meta", r#"{"model":"grok-4","type":"model"}"#),
            ("user", "second question"),
            ("assistant", "second answer"),
        ])
    );
    assert_eq!(read.drafts[0].body, support::user("look", vec![pixels()]));
    assert_eq!(
        read.drafts[4].body,
        support::result("call_read", "no such file", vec![], true)
    );
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T09:00:00.810Z")
    );
}

#[test]
fn openhands_reads_events_in_index_order_up_to_the_first_gap() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let conversations = dir.path().join("conversations");
    let conversation = conversations.join("99e61c51f6e945cc93b022fee3a459f4");
    let text = |text: &str| json!({"type": "text", "text": text});
    let events = [
        json!({"kind": "SystemPromptEvent", "source": "agent", "system_prompt": text("prompt"), "tools": []}),
        json!({"kind": "MessageEvent", "source": "user", "llm_message": {"role": "user", "content": [text("look"), {"type": "image", "image_urls": ["data:image/png;base64,cGl4ZWxz"]}]},
               "extended_content": [text("skill context")]}),
        json!({"kind": "ActionEvent", "source": "agent", "thought": [text("reading both")], "reasoning_content": "hmm", "tool_name": "terminal", "tool_call_id": "call_a",
               "tool_call": {"id": "call_a", "name": "terminal", "arguments": "{\"command\": \"cat a\"}", "origin": "completion"}, "llm_response_id": "r1",
               "action": {"command": "cat a", "kind": "TerminalAction"}}),
        json!({"kind": "ActionEvent", "source": "agent", "thought": [], "tool_name": "terminal", "tool_call_id": "call_b",
               "tool_call": {"id": "call_b", "name": "terminal", "arguments": "not json", "origin": "completion"}, "llm_response_id": "r1"}),
        json!({"kind": "ObservationEvent", "source": "environment", "tool_name": "terminal", "tool_call_id": "call_a", "action_id": "a1",
               "observation": {"content": [text("a body")], "is_error": false, "command": "cat a", "exit_code": 0, "kind": "TerminalObservation"}}),
        json!({"kind": "AgentErrorEvent", "source": "agent", "tool_name": "terminal", "tool_call_id": "call_b", "error": "bad arguments"}),
        json!({"kind": "ConversationStateUpdateEvent", "source": "environment", "key": "execution_status", "value": "finished"}),
        json!({"kind": "MessageEvent", "source": "agent", "llm_message": {"role": "assistant", "content": [text("done")]}, "llm_response_id": "r2"}),
        json!({"kind": "Condensation", "source": "environment", "forgotten_event_ids": [], "summary": "read two files", "summary_offset": 1, "llm_response_id": "r3"}),
    ];
    for (index, event) in events.iter().enumerate() {
        let mut event = event.clone();
        event["id"] = json!(format!("e{index}"));
        event["timestamp"] = json!(format!("2026-01-01T09:00:{index:02}.000123"));
        support::write(
            &conversation.join(format!("events/event-{index:05}-e{index}.json")),
            &event.to_string(),
        );
    }
    support::write(&conversation.join("events/event-00010-late.json"), &json!({"kind": "MessageEvent", "source": "user", "llm_message": {"role": "user", "content": [text("after a gap")]}}).to_string());
    support::write(&conversation.join("base_state.json"), &json!({"id": "99e61c51-f6e9-45cc-93b0-22fee3a459f4", "workspace": {"working_dir": "/workspace/openhands"}}).to_string());
    support::write(
        &conversations.join("not-a-conversation/events/event-00000-x.json"),
        "{}",
    );
    let adapter = OpenhandsAdapter::new(conversations);
    let native = only(&adapter);
    assert_eq!(native.id, "99e61c51-f6e9-45cc-93b0-22fee3a459f4");
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "look"),
            ("thinking", "hmm"),
            ("assistant", "reading both"),
            ("tool_call", r#"terminal {"command":"cat a"}"#),
            ("tool_call", r#"terminal {"input":"not json"}"#),
            ("tool_result", "a body"),
            ("tool_result", "bad arguments"),
            ("assistant", "done"),
            ("compaction", "read two files"),
        ])
    );
    assert_eq!(read.drafts[0].body, support::user("look", vec![pixels()]));
    assert_eq!(
        read.drafts[6].body,
        support::result("call_b", "bad arguments", vec![], true)
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/openhands"));
}

#[test]
fn vibe_reads_messages_from_the_last_compaction_boundary_on_as_vibe_replays_them() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let session = dir.path().join("vibe/session_20260101_090000_76f1b367");
    support::write(&session.join("attachments/card.png"), "pixels");
    let call = |id: &str, name: &str, arguments: &str| json!({"id": id, "index": 0, "function": {"name": name, "arguments": arguments}, "type": "function"});
    let messages = [
        json!({"role": "user", "content": "inspect @card.png", "images": [{"source": {"kind": "file", "path": "attachments/card.png"}, "alias": "card.png", "mime_type": "image/png"}],
               "injected": false, "message_id": "m1"}),
        json!({"role": "assistant", "content": "", "injected": false, "tool_calls": [call("call_a", "read_file", "{\"file_path\": \"a.txt\"}")], "message_id": "m2"}),
        json!({"role": "tool", "content": "<tool_error>read_file: missing</tool_error>", "injected": false, "name": "read_file", "tool_call_id": "call_a",
               "tool_result": {"output": {"error": "missing"}, "duration": 0.01, "cancelled": false}}),
        json!({"role": "assistant", "content": "it is missing", "reasoning_content": "hmm", "injected": false, "message_id": "m3"}),
        json!({"role": "user", "content": "<system-reminder>be brief</system-reminder>", "injected": true, "message_id": "m4"}),
        json!({"role": "user", "content": "<previous_user_messages>inspect</previous_user_messages><compaction_summary>looked for a.txt</compaction_summary>",
               "injected": true, "message_id": "m5", "context_boundary": "compaction"}),
        json!({"role": "user", "content": [{"type": "text", "text": "next"}], "injected": false, "message_id": "m6"}),
        json!({"role": "assistant", "content": "", "injected": false, "tool_calls": [call("call_b", "bash", "{\"command\": \"sleep 9\"}")], "message_id": "m7"}),
        json!({"role": "tool", "content": "<user_cancellation>Tool execution interrupted - no response available</user_cancellation>", "injected": false,
               "name": "bash", "tool_call_id": "call_b", "tool_result": {"output": null, "cancelled": true}}),
    ];
    support::write(&session.join("messages.jsonl"), &support::jsonl(&messages));
    support::write(
        &session.join("meta.json"),
        &json!({"session_id": "76f1b367-a336-6b84-96cb-66ccf903b3d5", "start_time": "2026-01-01T09:00:00.734402+00:00", "end_time": null,
                "environment": {"working_directory": "/workspace/vibe"}, "username": "me", "title": "Vibe fixture", "title_source": "manual",
                "total_messages": 9}).to_string(),
    );
    let adapter = VibeAdapter::new(dir.path().join("vibe"));
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Vibe fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "inspect @card.png"),
            ("tool_call", r#"read_file {"file_path":"a.txt"}"#),
            ("tool_result", "read_file: missing"),
            ("thinking", "hmm"),
            ("assistant", "it is missing"),
            (
                "compaction",
                "<previous_user_messages>inspect</previous_user_messages><compaction_summary>looked for a.txt</compaction_summary>"
            ),
            ("user", "next"),
            ("tool_call", r#"bash {"command":"sleep 9"}"#),
            (
                "tool_result",
                "Tool execution interrupted - no response available"
            ),
        ])
    );
    assert_eq!(
        read.drafts[0].body,
        support::user("inspect @card.png", vec![pixels()])
    );
    assert_eq!(
        read.drafts[2].body,
        support::result("call_a", "read_file: missing", vec![], true)
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/vibe"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T09:00:00.734Z")
    );
}

#[test]
fn qwen_reads_the_active_branch_after_a_rewind() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let session = "44c47f3e-ccfe-4fb7-aa06-0217dc950456";
    let record = |uuid: &str, parent: Option<&str>, second: u32, kind: &str, fields: Value| {
        let mut record = json!({"uuid": uuid, "parentUuid": parent, "sessionId": session, "timestamp": format!("2026-01-01T09:00:{second:02}.000Z"),
                                "type": kind, "cwd": "/workspace/qwen", "version": "0.22.1"});
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        record
    };
    let records = [
        record(
            "u1",
            None,
            1,
            "user",
            json!({"message": {"role": "user", "parts": [
                {"text": "look"}, {"inlineData": {"mimeType": "image/png", "data": "cGl4ZWxz"}},
                {"text": "<qwen:user-prompt-submit-context>\nhook output\n</qwen:user-prompt-submit-context>"},
            ]}}),
        ),
        record(
            "a1",
            Some("u1"),
            2,
            "assistant",
            json!({"model": "qwen3-coder", "message": {"role": "model", "parts": [
                {"text": "hmm", "thought": true, "thoughtSignature": "sig"},
                {"text": "reading"},
                {"functionCall": {"id": "call_1", "name": "read_file", "args": {"path": "a"}}},
            ]}}),
        ),
        record(
            "r1",
            Some("a1"),
            3,
            "tool_result",
            json!({"message": {"role": "user", "parts": [
            {"functionResponse": {"id": "call_1", "name": "read_file", "response": {"error": "missing"}}},
        ]}, "toolCallResult": {"callId": "call_1", "status": "error"}}),
        ),
        record(
            "n1",
            Some("r1"),
            4,
            "user",
            json!({"subtype": "notification", "message": {"role": "user", "parts": [{"text": "build done"}]}}),
        ),
        record(
            "x1",
            Some("n1"),
            5,
            "user",
            json!({"message": {"role": "user", "parts": [{"text": "abandoned"}]}}),
        ),
        record(
            "w1",
            Some("n1"),
            6,
            "system",
            json!({"subtype": "rewind", "systemPayload": {}}),
        ),
        record(
            "c1",
            Some("w1"),
            7,
            "system",
            json!({"subtype": "chat_compression", "systemPayload": {"compressedHistory": [
                {"role": "user", "parts": [{"text": "read a"}]},
                {"role": "model", "parts": [{"text": "Got it. Thanks for the additional context!"}, {"functionCall": {"id": "call_2", "name": "ls", "args": {}}}]},
            ]}}),
        ),
        record(
            "r2",
            Some("c1"),
            8,
            "tool_result",
            json!({"message": {"role": "user", "parts": [
            {"functionResponse": {"id": "call_2", "name": "ls", "response": {"output": "a"}, "parts": [{"inlineData": {"mimeType": "image/png", "data": "cGl4ZWxz"}}]}},
        ]}, "toolCallResult": {"callId": "call_2", "status": "success"}}),
        ),
        record(
            "t1",
            Some("r2"),
            9,
            "system",
            json!({"subtype": "custom_title", "systemPayload": {"customTitle": "Qwen fixture", "titleSource": "auto"}}),
        ),
        record(
            "s1",
            Some("t1"),
            9,
            "system",
            json!({"subtype": "session_artifact_event", "systemPayload": {}}),
        ),
    ];
    let root = dir.path().join("qwen");
    support::write(
        &root.join(format!("projects/-workspace-qwen/chats/{session}.jsonl")),
        &support::jsonl(&records),
    );
    support::write(
        &root.join(format!(
            "projects/-workspace-qwen/chats/{session}.ledger.jsonl"
        )),
        "{}\n",
    );
    let adapter = QwenAdapter::new(root);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "look"),
            ("meta", r#"{"model":"qwen3-coder","type":"model"}"#),
            ("thinking", "hmm"),
            ("assistant", "reading"),
            ("tool_call", r#"read_file {"path":"a"}"#),
            ("tool_result", "missing"),
            ("compaction", "read a"),
            ("tool_call", "ls {}"),
            ("tool_result", "a"),
        ])
    );
    assert_eq!(read.drafts[0].body, support::user("look", vec![pixels()]));
    assert_eq!(
        read.drafts[5].body,
        support::result("call_1", "missing", vec![], true)
    );
    assert_eq!(
        read.drafts[8].body,
        support::result("call_2", "a", vec![pixels()], false)
    );
    assert_eq!(read.info.title.as_deref(), Some("Qwen fixture"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/qwen"));
}

#[test]
fn kimi_reads_typed_turns_and_loop_events_and_follows_undo() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("kimi");
    let session =
        root.join("sessions/wd_kimi_0123456789ab/session_0e9113f5-23d9-4ad4-8a94-a3f44c175d14");
    support::write(&session.join("agents/main/blobs/b64hash"), "pixels");
    let mut time = 1_767_258_000_000_i64;
    let mut record = |kind: &str, fields: Value| {
        time += 1000;
        let mut record = json!({"type": kind, "agentId": "main", "time": time});
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        record
    };
    let text = |text: &str| json!({"type": "text", "text": text});
    let user = |content: Value, origin: Value| json!({"message": {"role": "user", "content": content, "toolCalls": [], "origin": origin}});
    let event = |event: Value| json!({"event": event});
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1_767_258_000_000_i64}),
        record(
            "runtime.set_binding",
            json!({"workspaceId": "wd_kimi_0123456789ab"}),
        ),
        record(
            "context.append_message",
            user(
                json!([text("look"), {"type": "image_url", "imageUrl": {"url": "blobref:image/png;b64hash"}}]),
                json!({"kind": "user"}),
            ),
        ),
        record(
            "context.append_message",
            user(
                json!([text("<system-reminder>be careful</system-reminder>")]),
                json!({"kind": "injection", "variant": "permission_mode"}),
            ),
        ),
        record(
            "context.append_loop_event",
            event(json!({"type": "step.begin", "uuid": "s1", "step": 1})),
        ),
        record(
            "context.append_loop_event",
            event(
                json!({"type": "content.part", "uuid": "p1", "part": {"type": "think", "think": "hmm"}}),
            ),
        ),
        record(
            "context.append_loop_event",
            event(json!({"type": "content.part", "uuid": "p2", "part": text("reading")})),
        ),
        record(
            "context.append_loop_event",
            event(
                json!({"type": "tool.call", "uuid": "c1", "toolCallId": "functions.Read:0", "name": "Read", "args": {"path": "a"}}),
            ),
        ),
        record(
            "context.append_loop_event",
            event(
                json!({"type": "tool.result", "toolCallId": "functions.Read:0", "result": {"output": "a body", "note": "<system>1 line read</system>"}}),
            ),
        ),
        record(
            "context.append_loop_event",
            event(json!({"type": "step.end", "uuid": "s1", "finishReason": "end_turn"})),
        ),
        record(
            "context.append_message",
            user(json!([text("abandoned")]), json!({"kind": "user"})),
        ),
        record(
            "context.append_message",
            json!({"message": {"role": "assistant", "content": [text("abandoned answer")], "toolCalls": []}}),
        ),
        record("context.undo", json!({"count": 1})),
        record(
            "context.apply_compaction",
            json!({"summary": "read a", "compactedCount": 5, "keptUserMessageCount": 1}),
        ),
        record(
            "context.append_message",
            user(json!([text("next")]), json!(null)),
        ),
        record(
            "context.append_message",
            json!({"message": {"role": "assistant", "content": [], "toolCalls": [{"type": "function", "id": "call_2", "name": "Bash", "arguments": "{\"command\":\"false\"}"}]}}),
        ),
        record(
            "context.append_message",
            json!({"message": {"role": "tool", "content": [text("exit 1")], "toolCalls": [], "toolCallId": "call_2", "isError": true}}),
        ),
    ];
    support::write(
        &session.join("agents/main/wire.jsonl"),
        &support::jsonl(&records),
    );
    support::write(
        &session.join("agents/agent-1/wire.jsonl"),
        &support::jsonl(&records[..3]),
    );
    support::write(
        &session.join("state.json"),
        &json!({"id": "session_0e9113f5-23d9-4ad4-8a94-a3f44c175d14", "version": 2, "title": "Kimi fixture", "titleKind": "custom", "isCustomTitle": true,
                "createdAt": 1_767_258_000_000_i64, "updatedAt": 1_767_258_017_000_i64, "cwd": "/workspace/kimi", "archived": false,
                "agents": {"main": {"homedir": "agents/main", "type": "main"}}, "custom": {}}).to_string(),
    );
    let child =
        root.join("sessions/wd_kimi_0123456789ab/session_11111111-2222-4333-8444-555555555555");
    support::write(
        &child.join("agents/main/wire.jsonl"),
        &support::jsonl(&records[..3]),
    );
    support::write(
        &child.join("state.json"),
        &json!({"id": "session_child", "custom": {"child_session_kind": "child"}}).to_string(),
    );
    let adapter = KimiAdapter::new(root);
    let native = only(&adapter);
    assert_eq!(native.title.as_deref(), Some("Kimi fixture"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "look"),
            ("thinking", "hmm"),
            ("assistant", "reading"),
            ("tool_call", r#"Read {"path":"a"}"#),
            ("tool_result", "a body\n<system>1 line read</system>"),
            ("compaction", "read a"),
            ("user", "next"),
            ("tool_call", r#"Bash {"command":"false"}"#),
            ("tool_result", "exit 1"),
        ])
    );
    assert_eq!(read.drafts[0].body, support::user("look", vec![pixels()]));
    assert_eq!(
        read.drafts[8].body,
        support::result("call_2", "exit 1", vec![], true)
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/kimi"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T09:00:00.000Z")
    );
}

#[test]
fn dsh_reads_the_latest_generation_and_visible_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("dsh");
    let session = root.join("sessions/--work-demo--/session-x");
    let records = [
        json!({"type": "session", "version": 4, "id": "session-x", "createdAt": 1_767_258_000_000_i64, "cwd": "/work/demo", "isSeeded": false, "delegationDepth": 0}),
        json!({"type": "turn/start", "seq": 0, "time": 1_767_258_000_001_i64, "data": {"turn": 1}}),
        json!({"type": "user/message", "seq": 1, "time": 1_767_258_000_002_i64, "data": {"id": "injected", "role": "user", "source": {"kind": "agent-instructions"}, "content": [{"type": "text", "text": "hidden instructions"}]}, "surfaceOp": "append"}),
        json!({"type": "user/message", "seq": 2, "time": 1_767_258_000_003_i64, "data": {"id": "user-1", "role": "user", "source": {"kind": "user"}, "content": [{"type": "text", "text": "look"}, {"type": "image", "attachment": {"attachmentId": format!("sha256:{PIXELS_SHA256}"), "mediaType": "image/png", "bytes": 6, "width": 0, "height": 0}}]}, "surfaceOp": "append"}),
        json!({"type": "step/start", "seq": 3, "time": 1_767_258_000_004_i64, "data": {"turn": 1, "step": 1}}),
        json!({"type": "assistant/message", "seq": 4, "time": 1_767_258_000_005_i64, "data": {"turn": 1, "step": 1, "message": {"id": "assistant-1", "role": "assistant", "source": {"kind": "model", "provider": "deepseek-official", "model": "deepseek-flash"}, "content": [{"type": "reasoning", "text": "hmm"}, {"type": "text", "text": "reading"}, {"type": "tool-call", "id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}, {"type": "tool-call", "id": "call_2", "name": "read", "arguments": "{\"path\":\"b\"}"}]}, "stream": []}, "surfaceOp": "append"}),
        json!({"type": "tool/call", "seq": 5, "time": 1_767_258_000_006_i64, "data": {"turn": 1, "step": 1, "callId": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}}),
        json!({"type": "tool/result", "seq": 6, "time": 1_767_258_000_007_i64, "data": {"turn": 1, "step": 1, "message": {"id": "result-1", "role": "tool", "source": {"kind": "tool", "callId": "call_1"}, "toolCallId": "call_1", "content": [{"type": "text", "text": "a body"}], "isError": false}}, "surfaceOp": "append"}),
        json!({"type": "tool/call", "seq": 7, "time": 1_767_258_000_008_i64, "data": {"turn": 1, "step": 1, "callId": "call_2", "name": "read", "arguments": "{\"path\":\"b\"}"}}),
        json!({"type": "tool/result", "seq": 8, "time": 1_767_258_000_009_i64, "data": {"turn": 1, "step": 1, "message": {"id": "result-2", "role": "tool", "source": {"kind": "tool", "callId": "call_2"}, "toolCallId": "call_2", "content": [{"type": "text", "text": "missing"}], "isError": true}}, "surfaceOp": "append"}),
        json!({"type": "tool/result", "seq": 9, "time": 1_767_258_000_010_i64, "data": {"turn": 1, "step": 1, "message": {"id": "result-1", "role": "tool", "source": {"kind": "tool", "callId": "call_1"}, "toolCallId": "call_1", "content": [{"type": "text", "text": "pruned"}], "isError": false}}, "surfaceOp": {"op": "replace", "startSeq": 6, "endSeq": 6}, "sourceEventSeqs": [6]}),
        json!({"type": "step/end", "seq": 10, "time": 1_767_258_000_011_i64, "data": {"turn": 1, "step": 1}}),
        json!({"type": "turn/end", "seq": 11, "time": 1_767_258_000_012_i64, "data": {"turn": 1, "reason": {"kind": "completed"}}}),
        json!({"type": "compaction/start", "seq": 12, "time": 1_767_258_000_013_i64, "data": {"compactionId": "compact-1", "turn": null}}),
        json!({"type": "compaction/summary", "seq": 13, "time": 1_767_258_000_014_i64, "data": {"compactionId": "compact-1", "summary": [{"type": "text", "text": "read a"}, {"type": "text", "text": "b was missing"}], "shadowedRange": {"start": 1, "end": 8}, "shadowedSeqs": [1, 2, 4, 9, 8], "shadowedTokenCount": 0, "provider": "deepseek-official", "model": "deepseek-flash"}}),
        json!({"type": "user/message", "seq": 14, "time": 1_767_258_000_015_i64, "data": {"id": "checkpoint-1", "role": "user", "source": {"kind": "compact-checkpoint", "compactionId": "compact-1"}, "content": [{"type": "text", "text": "<compacted-summary>"}, {"type": "text", "text": "read a"}, {"type": "text", "text": "b was missing"}, {"type": "text", "text": "</compacted-summary>"}]}, "surfaceOp": {"op": "replace", "startSeq": 1, "endSeq": 8}, "sourceEventSeqs": [12, 13, 1, 2, 4, 9, 8]}),
        json!({"type": "compaction/end", "seq": 15, "time": 1_767_258_000_016_i64, "data": {"compactionId": "compact-1", "turn": null}}),
        json!({"type": "session/title", "seq": 16, "time": 1_767_258_000_017_i64, "data": {"title": "Dsh fixture", "messageSeqs": [], "source": {"kind": "user"}}}),
    ];
    support::write(&session.join("session.v4.jsonl"), &support::jsonl(&records));
    support::write(
        &session.join("session.v3.jsonl"),
        &support::jsonl(&[
            json!({"type": "session", "version": 3, "id": "old-session", "createdAt": 1_767_258_000_000_i64}),
        ]),
    );
    support::write(
        &root.join("sessions/--work-demo--/session-child/session.v4.jsonl"),
        &support::jsonl(&[
            json!({"type": "session", "version": 4, "id": "session-child", "createdAt": 1_767_258_000_000_i64, "cwd": "/work/demo", "isSeeded": false, "origin": "subagent", "delegationDepth": 1}),
        ]),
    );
    support::write(
        &root.join(format!(
            "attachments/v1/objects/{}/{PIXELS_SHA256}",
            &PIXELS_SHA256[..2]
        )),
        "pixels",
    );
    let adapter = DshAdapter::new(root);
    let native = only(&adapter);
    assert_eq!(native.id, "session-x");
    assert_eq!(native.path, session.join("session.v4.jsonl"));
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(
        support::bodies(&read.drafts),
        vec![
            support::user("look", vec![pixels()]),
            support::meta(json!({"type": "model", "model": "deepseek-flash"})),
            support::thinking("hmm", None),
            support::assistant("reading"),
            support::call("call_1", "read", json!({"path": "a"})),
            support::call("call_2", "read", json!({"path": "b"})),
            support::result("call_1", "a body", vec![], false),
            support::result("call_2", "missing", vec![], true),
            support::compaction("read a\nb was missing"),
        ]
    );
    assert_eq!(read.info.title.as_deref(), Some("Dsh fixture"));
    assert_eq!(read.info.cwd.as_deref(), Some("/work/demo"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-01-01T09:00:00.000Z")
    );
}

#[test]
fn muse_reads_run_events_including_those_inside_retained_frames() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let session = "74747474-7474-4747-8747-747474747474";
    let mut sequence = 0;
    let mut envelope = |kind: &str, payload: Value| {
        sequence += 1;
        json!({"schema_version": 1, "id": format!("e{sequence}"), "stream": {"kind": "session", "id": session}, "sequence": sequence,
               "recorded_at": 1_767_258_000_000_000_i64 + sequence * 1_000_000, "record_type": "event", "durability": "durable",
               "payload_type": kind, "payload_schema_version": 1, "payload": payload})
    };
    let run = |event: Value| json!({"event": event, "kind": "run", "run_id": "r1"});
    let permission = envelope(
        "runtime.session",
        json!({"kind": "security_mode", "mode": "normal"}),
    );
    let lines = [
        json!({"retained_frame": "session_permission_transaction", "children": [{"child_index": 0, "record_json": permission.to_string()}]}),
        envelope(
            "runtime.session.metadata",
            json!({"kind": "metadata", "record": {"workspace_root": "/workspace/muse", "model_id": "glimmer"}}),
        ),
        envelope(
            "runtime.user_intent.accepted",
            json!({"intent_id": "r1", "model_messages": [{"content": [{"kind": "text", "text": "read a"}]}]}),
        ),
        envelope(
            "runtime.session",
            run(json!({"kind": "started", "prompt": "read a"})),
        ),
        json!({"schema_version": 1, "retained_marker": "omitted_live_only", "stream": {"kind": "session", "id": session}}),
        envelope(
            "runtime.session",
            run(json!({"kind": "reasoning_summary_committed", "text": "hmm"})),
        ),
        envelope(
            "runtime.session",
            run(
                json!({"kind": "assistant_tool_calls_committed", "message_id": "m1",
                                               "tool_calls": [{"args": "{\"path\":\"a\"}", "call_id": "call_1", "id": "call_1", "name": "read_file"}]}),
            ),
        ),
        envelope(
            "runtime.session",
            run(
                json!({"kind": "tool_result_batch_committed", "batch_id": "m1",
                                               "results": [{"text": "tool failed: missing", "tool_call_id": "call_1", "tool_call_index": 0}]}),
            ),
        ),
        envelope(
            "runtime.session",
            run(
                json!({"kind": "assistant_message_committed", "message_id": "m2", "text": "it is missing"}),
            ),
        ),
        envelope(
            "runtime.session",
            run(json!({"kind": "terminal", "terminal": "completed"})),
        ),
        envelope(
            "session.name.changed",
            json!({"session_id": session, "new_name": "quiet-lyra", "source": "automatic"}),
        ),
        envelope(
            "session.name.changed",
            json!({"session_id": session, "new_name": "Muse fixture", "source": "user"}),
        ),
    ];
    let root = dir.path().join("muse");
    support::write(
        &root.join(format!("sessions/2026/01/01/{session}/session.jsonl")),
        &support::jsonl(&lines),
    );
    support::write(
        &root.join(format!(
            "sessions/2026/01/01/{session}/subagent/child/session.jsonl"
        )),
        &support::jsonl(&lines[1..4]),
    );
    let adapter = MuseAdapter::new(root);
    let read = read(&adapter, &blobs);
    assert_eq!(
        kinds(&read.drafts),
        pairs(&[
            ("user", "read a"),
            ("thinking", "hmm"),
            ("tool_call", r#"read_file {"path":"a"}"#),
            ("tool_result", "tool failed: missing"),
            ("assistant", "it is missing"),
        ])
    );
    assert_eq!(
        read.drafts[3].body,
        support::result("call_1", "tool failed: missing", vec![], true)
    );
    assert_eq!(read.info.title.as_deref(), Some("Muse fixture"));
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/muse"));
}
