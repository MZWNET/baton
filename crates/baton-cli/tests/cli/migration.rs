use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use baton_core::{Agent, Assistant, Body, Draft, Event, ToolCall, ToolResult, User};
use serde_json::{Value, json};

use super::support::Home;
use super::{ask, entry, git_project, header, transcript_cwd, write_pi};

fn store(home: &Home, session: &str, events: &[Event], record: Value) {
    let directory = home.session(session);
    fs::create_dir_all(directory.join("events")).unwrap();
    fs::create_dir_all(directory.join("records")).unwrap();
    let mut lines = Vec::new();
    for event in events {
        serde_json::to_writer(&mut lines, event).unwrap();
        lines.push(b'\n');
    }
    fs::write(
        directory.join("events/test-018cc251-f400-7000-8000-000000000001.jsonl.zst"),
        zstd::bulk::compress(&lines, 1).unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join("records/test-018cc251-f400-7000-8000-000000000001.json"),
        format!("{record}\n"),
    )
    .unwrap();
}

fn draft(second: u32, id: &str, body: Body) -> Draft {
    Draft {
        timestamp: format!("2026-01-01T00:00:{second:02}.000Z"),
        native_entry_id: Some(id.into()),
        body,
    }
}

fn session_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);

    files
}

#[test]
fn old_pi_call_ids_migrate_without_adding_a_branch() {
    let home = Home::new();
    let events = Event::chain(
        [
            draft(
                1,
                "a1",
                Body::User(User {
                    text: "question".into(),
                    images: vec![],
                }),
            ),
            draft(
                2,
                "a2",
                Body::ToolCall(ToolCall {
                    call_id: "call_X_fc_Y".into(),
                    name: "read".into(),
                    input: json!({"path":"file"}),
                }),
            ),
            draft(
                3,
                "a3",
                Body::ToolResult(ToolResult {
                    call_id: "call_X_fc_Y".into(),
                    text: "tool output".into(),
                    images: vec![],
                    is_error: false,
                }),
            ),
            draft(
                4,
                "a4",
                Body::ToolCall(ToolCall {
                    call_id: "call_Z_ctc_W".into(),
                    name: "read".into(),
                    input: json!({"path":"other"}),
                }),
            ),
            draft(
                5,
                "a5",
                Body::ToolResult(ToolResult {
                    call_id: "call_Z_ctc_W".into(),
                    text: "other output".into(),
                    images: vec![],
                    is_error: false,
                }),
            ),
            draft(
                6,
                "a6",
                Body::ToolCall(ToolCall {
                    call_id: "call_function_abcdefghijkl_1".into(),
                    name: "read".into(),
                    input: json!({"path":"portable"}),
                }),
            ),
            draft(
                7,
                "a7",
                Body::ToolResult(ToolResult {
                    call_id: "call_function_abcdefghijkl_1".into(),
                    text: "portable output".into(),
                    images: vec![],
                    is_error: false,
                }),
            ),
            draft(
                8,
                "a8",
                Body::Assistant(Assistant {
                    text: "answer".into(),
                }),
            ),
        ],
        None,
        Agent::Pi,
        "pi-1",
    );
    let host = home.host();
    store(
        &home,
        "old-pi",
        &events,
        json!({"cwd": "/work", "created": "2026-01-01T00:00:00.000Z", "link": {"host": host, "agent": "pi", "native_id": "pi-1", "path": home.pi_file(), "exported_events": 8, "direction": "import"}}),
    );
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "question"),
            entry(
                "a2",
                Some("a1"),
                2,
                json!({"role":"assistant", "content":[{"type":"toolCall", "id":"call_X|fc_Y", "name":"read", "arguments":{"path":"file"}}], "stopReason":"toolUse"}),
            ),
            entry(
                "a3",
                Some("a2"),
                3,
                json!({"role":"toolResult", "toolCallId":"call_X|fc_Y", "content":[{"type":"text", "text":"tool output"}], "isError":false}),
            ),
            entry(
                "a4",
                Some("a3"),
                4,
                json!({"role":"assistant", "content":[{"type":"toolCall", "id":"call_Z|ctc_W", "name":"read", "arguments":{"path":"other"}}], "stopReason":"toolUse"}),
            ),
            entry(
                "a5",
                Some("a4"),
                5,
                json!({"role":"toolResult", "toolCallId":"call_Z|ctc_W", "content":[{"type":"text", "text":"other output"}], "isError":false}),
            ),
            entry(
                "a6",
                Some("a5"),
                6,
                json!({"role":"assistant", "content":[{"type":"toolCall", "id":"call_function_abcdefghijkl_1", "name":"read", "arguments":{"path":"portable"}}], "stopReason":"toolUse"}),
            ),
            entry(
                "a7",
                Some("a6"),
                7,
                json!({"role":"toolResult", "toolCallId":"call_function_abcdefghijkl_1", "content":[{"type":"text", "text":"portable output"}], "isError":false}),
            ),
            entry(
                "a8",
                Some("a7"),
                8,
                json!({"role":"assistant", "content":[{"type":"text", "text":"answer"}], "stopReason":"stop"}),
            ),
        ],
    );
    assert!(!home.run(&["list"]).status.success());
    home.baton(&["migrate"]);
    home.baton(&["import", "--agent", "pi"]);
    let after = home.baton(&["show", "old-pi"]);
    assert_eq!(
        after
            .lines()
            .filter(|line| line.starts_with("branch "))
            .count(),
        0
    );
    assert!(
        after.contains("question")
            && after.contains("answer")
            && after.contains("tool output")
            && after.contains("other output")
            && after.contains("portable output")
    );
    assert_eq!(home.sessions(&[]), vec!["old-pi"]);
    assert_eq!(home.event_count("old-pi"), 8);
    let root = home.path("baton/sessions");
    let before = session_files(&root);
    home.baton(&["migrate"]);
    assert_eq!(session_files(&root), before);
}

#[test]
fn newer_format_is_refused_without_changing_version() {
    let home = Home::new();
    fs::create_dir_all(home.path("baton")).unwrap();
    fs::write(home.path("baton/version"), "3\n").unwrap();
    assert!(!home.run(&["list"]).status.success());
    assert!(!home.run(&["migrate"]).status.success());
    assert_eq!(
        fs::read_to_string(home.path("baton/version")).unwrap(),
        "3\n"
    );
}

#[test]
fn migration_corrects_remote_for_export_resolution() {
    let home = Home::new();
    git_project(&home.project(), "https://example.com/Team/Project");
    let events = Event::chain(
        [draft(
            1,
            "a1",
            Body::User(User {
                text: "project question".into(),
                images: vec![],
            }),
        )],
        None,
        Agent::Pi,
        "pi-1",
    );
    store(
        &home,
        "old-project",
        &events,
        json!({"cwd":"/missing/project", "project":{"remote":"example.com/team/project", "root":null, "path":""}, "created":"2026-01-01T00:00:00.000Z"}),
    );
    home.baton_with_env(
        &["migrate"],
        &[("BATON_PROJECTS", home.project().as_os_str())],
    );
    let native = home.baton_with_env(
        &["export", "old-project", "--to", "claude"],
        &[("BATON_PROJECTS", home.project().as_os_str())],
    );
    let transcript = home.claude_transcript(native.trim());
    assert_eq!(
        fs::canonicalize(transcript_cwd(&transcript)).unwrap(),
        fs::canonicalize(home.project()).unwrap()
    );
    assert!(
        fs::read_to_string(transcript)
            .unwrap()
            .contains("project question")
    );
}
