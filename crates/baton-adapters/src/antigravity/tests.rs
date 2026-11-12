// The desktop app and the IDE keep their titles in protobuf caches, which these tests build with the crate's own encoder

use std::fs;
use std::path::Path;

use baton_core::{
    Adapter, Agent, Blobs, Body, Draft, Event, Export, Source, ToolCall, ToolResult, User, replay,
};
use rusqlite::Connection;
use serde_json::json;

use crate::antigravity::{AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter};
use crate::support::protobuf::{Encoder, Message};
use crate::support::{images, uri};

fn export_one(root: &Path, blobs: &Blobs, text: &str) -> String {
    let draft = baton_core::Draft {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        native_entry_id: None,
        body: Body::User(User {
            text: text.into(),
            images: vec![],
        }),
    };
    let events = Event::chain([draft], None, Agent::Pi, "source");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let models = replay::Models::new(&path);
    let export = Export {
        title: None,
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
        models: &models,
        seed: None,
    };
    AntigravityAdapter::new(root.into())
        .write(&export, blobs)
        .unwrap()
        .id
}

fn summary(title: &str, folder: &str) -> Encoder {
    Encoder::default()
        .string(1, title)
        .message(9, Encoder::default().string(1, folder))
}

#[test]
fn writes_native_images_and_reads_them_back_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let first = images::store(&blobs, "image/png", &[1, 2, 3]).unwrap();
    let second = images::store(&blobs, "image/jpeg", &[4, 5]).unwrap();
    let bodies = vec![
        Body::User(User {
            text: "look".into(),
            images: vec![first.clone(), second.clone()],
        }),
        Body::ToolCall(ToolCall {
            call_id: "call_1".into(),
            name: "view".into(),
            input: json!({}),
        }),
        Body::ToolResult(ToolResult {
            call_id: "call_1".into(),
            text: "shown".into(),
            images: vec![second.clone(), first.clone()],
            is_error: false,
        }),
    ];
    let drafts = bodies.iter().cloned().map(|body| Draft {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        native_entry_id: None,
        body,
    });
    let events = Event::chain(drafts, None, Agent::Pi, "source");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let models = replay::Models::new(&path);
    let export = Export {
        title: None,
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
        models: &models,
        seed: None,
    };
    let adapter = AntigravityAdapter::new(dir.path().join("antigravity"));
    let session = adapter.write(&export, &blobs).unwrap();
    let connection = Connection::open(&session.path).unwrap();
    let payload = |index| {
        connection
            .query_row(
                "SELECT step_payload FROM steps WHERE idx = ?1",
                [index],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap()
    };
    let user_bytes = payload(0);
    let user = Message::parse(&user_bytes).unwrap();
    assert!(user.bytes(47728).is_none());
    let input = user.message(19).unwrap();
    let attached: Vec<_> = input
        .repeated(5)
        .map(|bytes| Message::parse(bytes).unwrap())
        .collect();
    assert_eq!(
        attached
            .iter()
            .map(|image| (image.string(1).unwrap(), image.string(2).unwrap()))
            .collect::<Vec<_>>(),
        [("AQID", "image/png"), ("BAU=", "image/jpeg")]
    );
    let result_bytes = payload(2);
    let result_step = Message::parse(&result_bytes).unwrap();
    assert!(result_step.bytes(47728).is_none());
    let generic = result_step.message(140).unwrap();
    let result = generic.message(2).unwrap();
    let media: Vec<_> = result
        .repeated(4)
        .map(|bytes| Message::parse(bytes).unwrap())
        .collect();
    assert_eq!(
        media
            .iter()
            .map(|image| (image.string(1).unwrap(), image.bytes(2).unwrap()))
            .collect::<Vec<_>>(),
        [("image/jpeg", &[4, 5][..]), ("image/png", &[1, 2, 3][..])]
    );
    let read = adapter.read(&session, None, &blobs).unwrap();
    assert_eq!(
        read.drafts
            .into_iter()
            .map(|draft| draft.body)
            .collect::<Vec<_>>(),
        bodies
    );
}

#[test]
fn reads_native_image_fixture_and_legacy_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("antigravity");
    let id = export_one(&root, &blobs, "placeholder");
    let adapter = AntigravityAdapter::new(root);
    let session = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .find(|session| session.id == id)
        .unwrap();
    let connection = Connection::open(&session.path).unwrap();
    let file = dir.path().join("image file.png");
    fs::write(&file, [7, 8]).unwrap();
    let image_data = b"\x0a\x04AQID\x12\x09image/png";
    let media = b"\x0a\x09image/png\x12\x03\x01\x02\x03";
    let input = Encoder::default()
        .string(2, "look")
        .bytes(5, image_data)
        .message(
            5,
            Encoder::default()
                .string(2, "image/png")
                .string(4, &uri::from_path(file.to_str().unwrap())),
        );
    let legacy = Encoder::default().message(
        1,
        Encoder::default()
            .string(1, "image/png")
            .bytes(2, &[1, 2, 3]),
    );
    let user = Encoder::default()
        .varint(1, 14)
        .message(19, input)
        .message(47728, legacy)
        .finish();
    connection
        .execute("UPDATE steps SET step_payload = ?1 WHERE idx = 0", [&user])
        .unwrap();
    let call = Encoder::default()
        .string(1, "call_1")
        .string(2, "view")
        .string(3, "{}");
    let planner = Encoder::default()
        .varint(1, 15)
        .message(20, Encoder::default().message(7, call))
        .finish();
    let result = Encoder::default()
        .string(1, "shown")
        .bytes(4, media)
        .message(
            4,
            Encoder::default()
                .string(1, "image/png")
                .string(5, &uri::from_path(file.to_str().unwrap())),
        );
    let legacy = Encoder::default().message(
        1,
        Encoder::default()
            .string(1, "image/png")
            .bytes(2, &[1, 2, 3]),
    );
    let step = Encoder::default()
        .varint(1, 132)
        .message(
            5,
            Encoder::default().message(4, Encoder::default().string(1, "call_1")),
        )
        .message(140, Encoder::default().message(2, result))
        .message(47728, legacy)
        .finish();
    connection
        .execute(
            "INSERT INTO steps (idx, status, step_payload) VALUES (1, 3, ?1)",
            [&planner],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO steps (idx, status, step_payload) VALUES (2, 3, ?1)",
            [&step],
        )
        .unwrap();
    let legacy = Encoder::default().message(
        1,
        Encoder::default()
            .string(1, "image/png")
            .bytes(2, &[1, 2, 3]),
    );
    let legacy_user = Encoder::default()
        .varint(1, 14)
        .message(19, Encoder::default().string(2, "legacy"))
        .message(47728, legacy)
        .finish();
    connection
        .execute(
            "INSERT INTO steps (idx, status, step_payload) VALUES (3, 3, ?1)",
            [&legacy_user],
        )
        .unwrap();
    let mcp_call = Encoder::default()
        .string(1, "call_2")
        .string(2, "mcp")
        .string(3, "{}");
    let mcp = Encoder::default()
        .message(2, mcp_call)
        .string(3, "mcp result")
        .bytes(5, image_data)
        .bytes(6, media);
    let mcp_step = Encoder::default().varint(1, 38).message(47, mcp).finish();
    connection
        .execute(
            "INSERT INTO steps (idx, status, step_payload) VALUES (4, 3, ?1)",
            [&mcp_step],
        )
        .unwrap();
    let read = adapter.read(&session, None, &blobs).unwrap();
    let first = images::store(&blobs, "image/png", &[1, 2, 3]).unwrap();
    let second = images::store(&blobs, "image/png", &[7, 8]).unwrap();
    let Body::User(user) = &read.drafts[0].body else {
        panic!("expected user")
    };
    assert_eq!(user.images, [first.clone(), second.clone()]);
    let Body::ToolResult(result) = &read.drafts[2].body else {
        panic!("expected tool result")
    };
    assert_eq!(result.images, [first.clone(), second]);
    let Body::User(user) = &read.drafts[3].body else {
        panic!("expected legacy user")
    };
    assert_eq!(user.images, std::slice::from_ref(&first));
    let Body::ToolResult(result) = &read.drafts[5].body else {
        panic!("expected MCP result")
    };
    assert_eq!(result.images, [first.clone(), first]);

    let missing = dir.path().join("missing.png");
    let input = Encoder::default().string(2, "missing").message(
        5,
        Encoder::default()
            .string(2, "image/png")
            .string(4, &uri::from_path(missing.to_str().unwrap())),
    );
    let step = Encoder::default().varint(1, 14).message(19, input).finish();
    connection
        .execute("UPDATE steps SET step_payload = ?1 WHERE idx = 0", [&step])
        .unwrap();
    let read = adapter.read(&session, None, &blobs).unwrap();
    let Body::User(user) = &read.drafts[0].body else {
        panic!("expected user")
    };
    assert!(user.images.is_empty());

    let input = Encoder::default().string(2, "bad file").message(
        5,
        Encoder::default()
            .string(2, "image/png")
            .string(4, &uri::from_path(dir.path().to_str().unwrap())),
    );
    let step = Encoder::default().varint(1, 14).message(19, input).finish();
    connection
        .execute("UPDATE steps SET step_payload = ?1 WHERE idx = 0", [&step])
        .unwrap();
    assert!(adapter.read(&session, None, &blobs).is_err());

    let input = Encoder::default()
        .string(2, "bad encoding")
        .message(5, Encoder::default().string(1, "!").string(2, "image/png"));
    let step = Encoder::default().varint(1, 14).message(19, input).finish();
    connection
        .execute("UPDATE steps SET step_payload = ?1 WHERE idx = 0", [&step])
        .unwrap();
    assert!(adapter.read(&session, None, &blobs).is_err());
}

#[test]
fn desktop_reads_titles_from_its_cache_and_skips_archived_conversations() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("antigravity");
    let kept = export_one(&root, &blobs, "kept question");
    let archived = export_one(&root, &blobs, "archived question");
    let cache = Encoder::default()
        .message(
            1,
            Encoder::default()
                .string(1, &kept)
                .message(2, summary("Stale title", "file:///w")),
        )
        .message(
            1,
            Encoder::default()
                .string(1, &kept)
                .message(2, summary("Desktop title", "file:///workspace/desktop")),
        )
        .message(
            1,
            Encoder::default()
                .string(1, &archived)
                .message(2, summary("Archived", "file:///w")),
        );
    fs::write(root.join("agyhub_summaries_proto.pb"), cache.finish()).unwrap();
    fs::create_dir_all(root.join("annotations")).unwrap();
    fs::write(
        root.join("annotations").join(format!("{archived}.pbtxt")),
        "archived:true  archival_status_timestamp:{seconds:1782708366  nanos:907334000}",
    )
    .unwrap();
    let adapter = AntigravityDesktopAdapter::new(root);
    let sessions = adapter.discover(None).unwrap();
    assert_eq!(sessions.len(), 1, "archived conversations stay out");
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Desktop title"),
        "a later cache entry wins"
    );
    let read = adapter.read(&sessions[0], None, &blobs).unwrap();
    assert_eq!(read.drafts[0].body.text(), "kept question");
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/desktop"));
}

#[test]
fn ide_pairs_results_with_calls_in_order_and_titles_from_the_editor_state() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("antigravity-ide");
    let user_data = dir.path().join("Antigravity");
    let id = "0e9113f5-23d9-4ad4-8a94-a3f44c175d14";
    let records = [
        json!({"type": "USER_INPUT", "content": "<USER_REQUEST>read both</USER_REQUEST>\n<ADDITIONAL_METADATA>ide</ADDITIONAL_METADATA>",
               "created_at": "2026-01-01T09:00:00Z", "workspacePaths": ["/workspace/ide"]}),
        json!({"step_index": 1, "type": "PLANNER_RESPONSE", "content": "reading", "thinking": "hmm", "created_at": "2026-01-01T09:00:01Z",
               "tool_calls": [{"name": "view_file", "args": {"path": "a"}}, {"name": "view_file", "args": {"path": "b"}}]}),
        json!({"step_index": 2, "type": "VIEW_FILE", "content": "a body", "created_at": "2026-01-01T09:00:02Z"}),
        json!({"step_index": 3, "type": "VIEW_FILE", "content": "b body", "created_at": "2026-01-01T09:00:03Z"}),
        json!({"step_index": 4, "type": "CHECKPOINT", "content": "saved", "created_at": "2026-01-01T09:00:04Z"}),
    ];
    let log: String = records.iter().map(|record| format!("{record}\n")).collect();
    let logs = root.join("brain").join(id).join(".system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join("transcript.jsonl"), log).unwrap();
    let row = Encoder::default().string(
        1,
        &images::encode(&summary("IDE title", "file:///workspace/ide").finish()),
    );
    let topic = Encoder::default().message(1, Encoder::default().string(1, id).message(2, row));
    let state = user_data.join("User/globalStorage");
    fs::create_dir_all(&state).unwrap();
    let connection = Connection::open(state.join("state.vscdb")).unwrap();
    connection
        .execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
        .unwrap();
    connection
        .execute(
            "INSERT INTO ItemTable (key, value) VALUES ('antigravityUnifiedStateSync.trajectorySummaries', ?1)",
            [images::encode(&topic.finish())],
        )
        .unwrap();
    let adapter = AntigravityIdeAdapter::new(root, user_data);
    let sessions = adapter.discover(None).unwrap();
    assert_eq!(sessions[0].title.as_deref(), Some("IDE title"));
    let read = adapter.read(&sessions[0], None, &blobs).unwrap();
    let outline: Vec<String> = read
        .drafts
        .iter()
        .map(|draft| format!("{}: {}", draft.body.kind(), draft.body.text()))
        .collect();
    assert_eq!(
        outline,
        [
            "user: read both",
            "thinking: hmm",
            "assistant: reading",
            r#"tool_call: view_file {"path":"a"}"#,
            r#"tool_call: view_file {"path":"b"}"#,
            "tool_result: a body",
            "tool_result: b body",
        ]
    );
    for (call, result) in [(3, 5), (4, 6)] {
        let Body::ToolCall(call) = &read.drafts[call].body else {
            panic!("expected a call");
        };
        let Body::ToolResult(result) = &read.drafts[result].body else {
            panic!("expected a result");
        };
        assert_eq!(
            result.call_id, call.call_id,
            "results answer calls in order"
        );
    }
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/ide"));
}
