// Native stores are built with the crate's own protobuf encoder, shaped like the ones Cursor Agent 2026.03 to 2026.09 and the editor write

use std::fs;
use std::path::{Path, PathBuf};

use baton_core::{
    Adapter, Agent, Assistant, Blobs, Body, Compaction, Draft, Event, Export, Image, NativeSession,
    Source, ToolCall, ToolResult, User, replay,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cursor::{CursorAdapter, CursorIdeAdapter};
use crate::support::images;
use crate::support::protobuf::Encoder;

// Blobs named by their SHA-256, as both stores keep them
#[derive(Default)]
struct Graph {
    blobs: Vec<(String, Vec<u8>)>,
}

impl Graph {
    fn put(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let id = Sha256::digest(&bytes).to_vec();
        self.blobs
            .push((base16ct::lower::encode_string(&id), bytes));
        id
    }

    fn json(&mut self, message: Value) -> Vec<u8> {
        self.put(message.to_string().into_bytes())
    }
}

fn store(root: &Path, id: &str, meta: Value, graph: &Graph) -> PathBuf {
    let store = root
        .join("chats/632ceb05b38b16cd23391476ee3aa80e")
        .join(id)
        .join("store.db");
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    let connection = Connection::open(&store).unwrap();
    connection
        .execute_batch("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB); CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    for (id, data) in &graph.blobs {
        connection
            .execute(
                "INSERT OR REPLACE INTO blobs (id, data) VALUES (?1, ?2)",
                params![id, data],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO meta (key, value) VALUES ('0', ?1)",
            [base16ct::lower::encode_string(meta.to_string().as_bytes())],
        )
        .unwrap();
    store
}

fn outline(drafts: &[Draft]) -> Vec<String> {
    drafts
        .iter()
        .map(|draft| format!("{}: {}", draft.body.kind(), draft.body.text()))
        .collect()
}

fn bodies(drafts: &[Draft]) -> Vec<Body> {
    drafts.iter().map(|draft| draft.body.clone()).collect()
}

#[test]
fn cli_reads_the_model_history_with_times_from_the_turns() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let pixels = images::encode(b"pixels");
    let mut graph = Graph::default();
    let messages = [
        graph.json(json!({"role": "system", "content": "You are an AI coding assistant, powered by Composer."})),
        graph.json(json!({"role": "user", "content": "<user_info>\nWorkspace Path: /workspace/cursor\n</user_info>"})),
        graph.json(json!({"role": "user", "content": [
            {"type": "text", "text": "<timestamp>Monday, Aug 31, 2026, 8:26 PM (UTC-4)</timestamp>\n<user_query>\nlook at this\n</user_query>"},
            {"type": "image", "image": pixels, "mimeType": "image/png"},
        ], "providerOptions": {"cursor": {"requestId": "request-1"}}})),
        graph.json(json!({"role": "assistant", "content": [
            {"type": "redacted-reasoning", "data": "sealed", "providerOptions": {"cursor": {"modelName": "composer-2.5"}}},
            {"type": "text", "text": "checking"},
            {"type": "tool-call", "toolCallId": "tool_1", "toolName": "Read", "args": {"path": "card.png"}},
            {"type": "tool-call", "toolCallId": "tool_2", "toolName": "Read", "args": {"path": "tone.wav"}},
        ], "id": "1"})),
        graph.json(json!({"role": "tool", "id": "tool_1", "content": [{"type": "tool-result", "toolCallId": "tool_1", "toolName": "Read", "result": "Read image file",
            "experimental_content": [{"type": "text", "text": "Read image file"}, {"type": "image", "data": pixels, "mimeType": "image/png"}]}]})),
        graph.json(json!({"role": "tool", "id": "tool_2", "content": [{"type": "tool-result", "toolCallId": "tool_2", "toolName": "Read", "result": "Error: binary file"}],
            "providerOptions": {"cursor": {"highLevelToolCallResult": {"isError": true}}}})),
        graph.json(json!({"role": "assistant", "content": [{"type": "text", "text": "done"}], "id": "1"})),
    ];
    let user = graph.put(
        Encoder::default()
            .string(1, "look at this")
            .varint(25, 1_788_222_400_000)
            .finish(),
    );
    let thinking = graph.put(
        Encoder::default()
            .message(3, Encoder::default().string(1, "readable summary"))
            .finish(),
    );
    let reply = graph.put(
        Encoder::default()
            .message(
                1,
                Encoder::default()
                    .string(1, "checking")
                    .varint(2, 1_788_222_401_000),
            )
            .finish(),
    );
    let call = Encoder::default()
        .string(57, "tool_1")
        .varint(59, 1_788_222_402_000)
        .varint(60, 1_788_222_403_000);
    let call = graph.put(Encoder::default().message(2, call).finish());
    let turn = Encoder::default()
        .bytes(1, &user)
        .bytes(2, &thinking)
        .bytes(2, &reply)
        .bytes(2, &call)
        .string(3, "request-1");
    let turn = graph.put(Encoder::default().message(1, turn).finish());
    let mut root = Encoder::default();
    for message in &messages {
        root = root.bytes(1, message);
    }
    let root = graph.put(
        root.bytes(8, &turn)
            .string(9, "file:///workspace/cursor")
            .varint(26, 1_788_222_396_000)
            .finish(),
    );
    let root = base16ct::lower::encode_string(&root);
    let id = "760c3e81-113f-4000-aae9-fbfcefc67ce7";
    let meta = json!({"agentId": id, "latestRootBlobId": root, "name": "New Agent", "createdAt": 1_788_222_396_428_i64, "mode": "auto-run"});
    store(&dir.path().join("cursor"), id, meta, &graph);
    let child = "0b9f1f2e-5d6c-4c1a-9a51-3f1e8f0a7c11";
    let meta = json!({"agentId": child, "latestRootBlobId": root, "name": "Explore", "subagentInfo": {"parentAgentId": id}});
    store(&dir.path().join("cursor"), child, meta, &graph);

    let adapter = CursorAdapter::new(dir.path().join("cursor"));
    let sessions = adapter.discover(None).unwrap();
    assert_eq!(sessions.len(), 1, "subagent chats stay out");
    assert_eq!(
        sessions[0].title, None,
        "Cursor's placeholder name is no title"
    );
    let read = adapter.read(&sessions[0], None, &blobs).unwrap();
    assert_eq!(
        outline(&read.drafts),
        [
            "user: look at this",
            r#"meta: {"model":"composer-2.5","type":"model"}"#,
            "thinking: ",
            "assistant: checking",
            r#"tool_call: Read {"path":"card.png"}"#,
            r#"tool_call: Read {"path":"tone.wav"}"#,
            "tool_result: Read image file",
            "tool_result: Error: binary file",
            "assistant: done",
        ]
    );
    let times: Vec<&str> = read
        .drafts
        .iter()
        .map(|draft| &draft.timestamp[11..19])
        .collect();
    assert_eq!(
        times,
        [
            "00:26:40", "00:26:40", "00:26:40", "00:26:41", "00:26:42", "00:26:42", "00:26:43",
            "00:26:43", "00:26:43"
        ],
        "turns time the messages and calls, and the rest takes the time before"
    );
    let (Body::User(user), Body::ToolResult(image), Body::ToolResult(failed)) = (
        &read.drafts[0].body,
        &read.drafts[6].body,
        &read.drafts[7].body,
    ) else {
        panic!("expected a user message and two results");
    };
    assert_eq!(user.images, image.images, "both images are the same pixels");
    assert_eq!(user.images.len(), 1);
    assert!(failed.is_error && !image.is_error);
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/cursor"));
    assert_eq!(
        read.info.created.as_deref(),
        Some("2026-09-01T00:26:36.428Z")
    );
}

fn write(adapter: &CursorAdapter, blobs: &Blobs, bodies: Vec<Body>) -> NativeSession {
    let drafts = bodies.into_iter().map(|body| Draft {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        native_entry_id: None,
        body,
    });
    let events = Event::chain(drafts, None, Agent::Pi, "source");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let models = replay::Models::new(&path);
    let export = Export {
        title: Some("Imported"),
        cwd: Path::new("/workspace/cli"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
        models: &models,
        seed: None,
    };
    adapter.write(&export, blobs).unwrap()
}

fn ide_database(user_data: &Path) -> Connection {
    let state = user_data.join("User/globalStorage");
    fs::create_dir_all(&state).unwrap();
    let connection = Connection::open(state.join("state.vscdb")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER,
                                           isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT);",
        )
        .unwrap();
    connection
}

fn header(connection: &Connection, composer: &str, workspace: &str, subagent: bool, value: Value) {
    connection
        .execute(
            "INSERT INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, value)
             VALUES (?1, ?2, 1767225600000, 1767225601000, 0, ?3, 1767225601000, ?4)",
            params![composer, workspace, subagent, value.to_string()],
        )
        .unwrap();
}

fn put(connection: &Connection, key: &str, value: &Value) {
    connection
        .execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            params![key, value.to_string()],
        )
        .unwrap();
}

#[test]
fn ide_reads_agent_chats_through_the_graph_cursor_agent_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let cli = CursorAdapter::new(dir.path().join("cursor"));
    let image = Image {
        blob: blobs.put(b"pixels").unwrap(),
        mime: "image/png".into(),
    };
    let written = write(
        &cli,
        &blobs,
        vec![
            Body::User(User {
                text: "look".into(),
                images: vec![image.clone()],
            }),
            Body::Assistant(Assistant {
                text: "reading".into(),
            }),
            Body::ToolCall(ToolCall {
                call_id: "call_1".into(),
                name: "Read".into(),
                input: json!({"path": "a"}),
            }),
            Body::ToolResult(ToolResult {
                call_id: "call_1".into(),
                text: "body".into(),
                images: vec![],
                is_error: false,
            }),
            Body::Compaction(Compaction {
                summary: "so far".into(),
            }),
            Body::User(User {
                text: "next".into(),
                images: vec![],
            }),
        ],
    );
    let expected = vec![
        Body::User(User {
            text: "look".into(),
            images: vec![image],
        }),
        Body::Assistant(Assistant {
            text: "reading".into(),
        }),
        Body::ToolCall(ToolCall {
            call_id: "call_1".into(),
            name: "Read".into(),
            input: json!({"path": "a"}),
        }),
        Body::ToolResult(ToolResult {
            call_id: "call_1".into(),
            text: "body".into(),
            images: vec![],
            is_error: false,
        }),
        Body::Compaction(Compaction {
            summary: "so far".into(),
        }),
        Body::User(User {
            text: "next".into(),
            images: vec![],
        }),
    ];
    let store = Connection::open(&written.path).unwrap();
    let meta: String = store
        .query_row("SELECT value FROM meta", [], |row| row.get(0))
        .unwrap();
    let meta: Value = serde_json::from_slice(&base16ct::lower::decode_vec(meta).unwrap()).unwrap();
    let root: Vec<u8> = store
        .query_row(
            "SELECT data FROM blobs WHERE id = ?1",
            [meta["latestRootBlobId"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();

    let user_data = dir.path().join("Cursor");
    let connection = ide_database(&user_data);
    let mut statement = store.prepare("SELECT id, data FROM blobs").unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap();
    for row in rows {
        let (id, data) = row.unwrap();
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![format!("agentKv:blob:{id}"), data],
            )
            .unwrap();
    }
    let composer = "4f0c2b1e-8d7a-4c3b-9e6f-1a2b3c4d5e6f";
    let state = format!("~{}", images::encode(&root));
    put(
        &connection,
        &format!("composerData:{composer}"),
        &json!({"_v": 17, "composerId": composer, "name": "IDE chat", "isNAL": true, "conversationState": state}),
    );
    let claimed = json!({"type": "head", "composerId": composer, "name": "IDE chat", "workspaceIdentifier": {"id": "ws-1", "uri": {"fsPath": "/elsewhere"}}});
    header(&connection, composer, "ws-1", false, claimed);
    header(
        &connection,
        "child",
        "ws-1",
        true,
        json!({"name": "Subagent"}),
    );
    let workspace = user_data.join("User/workspaceStorage/ws-1");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join("workspace.json"),
        r#"{"folder": "file:///workspace/my%20ide"}"#,
    )
    .unwrap();

    let adapter = CursorIdeAdapter::new(user_data);
    let sessions = adapter.discover(None).unwrap();
    assert_eq!(sessions.len(), 1, "subagent composers stay out");
    assert_eq!(sessions[0].title.as_deref(), Some("IDE chat"));
    let read = adapter.read(&sessions[0], None, &blobs).unwrap();
    assert_eq!(bodies(&read.drafts), expected);
    assert_eq!(
        read.info.cwd.as_deref(),
        Some("/workspace/my ide"),
        "the workspace storage wins over the header's claim"
    );
}

#[test]
fn ide_reads_older_chats_from_their_bubbles() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let user_data = dir.path().join("Cursor");
    let connection = ide_database(&user_data);
    let composer = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
    let bubbles = [
        (
            "h1",
            1,
            json!({"type": 1, "text": "question", "createdAt": "2026-01-02T00:00:00Z"}),
        ),
        (
            "a1",
            2,
            json!({"type": 2, "text": "answer", "modelInfo": {"modelName": "claude-sonnet"}, "thinking": {"text": "pondering", "signature": "sig"}}),
        ),
        (
            "empty",
            2,
            json!({"type": 2, "modelInfo": {"modelName": "unused"}}),
        ),
        (
            "t1",
            2,
            json!({"type": 2, "text": "", "createdAt": "2026-01-02T00:00:05Z", "modelInfo": {"modelName": "claude-sonnet"},
            "toolFormerData": {"toolCallId": "call-1", "name": "read_file", "status": "completed", "params": "{\"path\":\"a\"}", "result": {"contents": "body"}}}),
        ),
        (
            "t2",
            2,
            json!({"type": 2, "modelInfo": {"modelName": "gpt-4"}, "toolFormerData": {"toolCallId": "call-2", "name": "run_terminal_cmd", "status": "error", "rawArgs": "{\"command\":\"ls\"}", "error": "denied"}}),
        ),
        (
            "default",
            2,
            json!({"type": 2, "text": "default picker", "modelInfo": {"modelName": "default"}}),
        ),
    ];
    let mut headers = Vec::new();
    for (id, kind, bubble) in &bubbles {
        put(&connection, &format!("bubbleId:{composer}:{id}"), bubble);
        headers.push(json!({"bubbleId": id, "type": kind}));
    }
    put(
        &connection,
        &format!("composerData:{composer}"),
        &json!({"_v": 10, "composerId": composer, "createdAt": 1767225600000_i64, "fullConversationHeadersOnly": headers}),
    );
    let value = json!({"name": "Old chat", "workspaceIdentifier": {"id": "ws-2", "uri": {"fsPath": "/workspace/old"}}});
    header(&connection, composer, "ws-2", false, value);

    let adapter = CursorIdeAdapter::new(user_data);
    let sessions = adapter.discover(None).unwrap();
    let read = adapter.read(&sessions[0], None, &blobs).unwrap();
    assert_eq!(
        outline(&read.drafts),
        [
            "user: question",
            r#"meta: {"model":"claude-sonnet","type":"model"}"#,
            "thinking: pondering",
            "assistant: answer",
            r#"tool_call: read_file {"path":"a"}"#,
            r#"tool_result: {"contents":"body"}"#,
            r#"meta: {"model":"gpt-4","type":"model"}"#,
            r#"tool_call: run_terminal_cmd {"command":"ls"}"#,
            "tool_result: denied",
            "assistant: default picker",
        ]
    );
    let Body::ToolResult(failed) = &read.drafts[8].body else {
        panic!("expected a result");
    };
    assert!(failed.is_error);
    let times: Vec<&str> = read
        .drafts
        .iter()
        .map(|draft| &draft.timestamp[..19])
        .collect();
    assert_eq!(
        times[3], "2026-01-02T00:00:00",
        "a bubble without a time takes the one before it"
    );
    assert_eq!(times[8], "2026-01-02T00:00:05");
    assert_eq!(
        read.info.cwd.as_deref(),
        Some("/workspace/old"),
        "without workspace storage the header's folder stands"
    );
    assert_eq!(read.info.title.as_deref(), Some("Old chat"));
}
