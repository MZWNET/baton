// The desktop app and the IDE keep their titles in protobuf caches, which these tests build with the crate's own encoder

use std::fs;
use std::path::Path;

use baton_core::{Adapter, Agent, Blobs, Body, Event, Export, Source, User, replay};
use rusqlite::Connection;
use serde_json::json;

use crate::antigravity::{AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter};
use crate::support::images;
use crate::support::protobuf::Encoder;

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
    let export = Export {
        title: None,
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
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
    let Body::ToolResult(second) = &read.drafts[6].body else {
        panic!("expected a result");
    };
    assert_eq!(
        second.call_id, "step1_1",
        "results answer calls in the order they were made"
    );
    assert_eq!(read.info.cwd.as_deref(), Some("/workspace/ide"));
}
