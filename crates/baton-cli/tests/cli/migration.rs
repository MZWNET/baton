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
    fs::write(home.path("baton/version"), "4\n").unwrap();
    assert!(!home.run(&["list"]).status.success());
    assert!(!home.run(&["migrate"]).status.success());
    assert_eq!(
        fs::read_to_string(home.path("baton/version")).unwrap(),
        "4\n"
    );
}

#[test]
fn migration_repairs_corrupted_ids_and_merges_the_fork() {
    let home = Home::new();
    let host = home.host();
    let hashed = baton_core::portable_call_id("call_X|fc_Y");
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
                    call_id: hashed.clone(),
                    name: "read".into(),
                    input: json!({"path":"file"}),
                }),
            ),
            draft(
                3,
                "a3",
                Body::ToolResult(ToolResult {
                    call_id: hashed,
                    text: "output".into(),
                    images: vec![],
                    is_error: false,
                }),
            ),
        ],
        None,
        Agent::Pi,
        "pi-1",
    );
    let claude_link = json!({"host":host, "agent":"claude", "native_id":"claude-1", "path":"/old/claude.jsonl", "exported_events":3, "exported_leaf":events[2].id, "direction":"export", "continued":true});
    store(
        &home,
        "parent",
        &events,
        json!({"created":"2026-01-01T00:00:00.000Z", "link":{"host":host,"agent":"pi","native_id":"pi-1","path":"/pi.jsonl","exported_events":3,"direction":"import"}}),
    );
    fs::write(
        home.session("parent/records/test-018cc251-f400-7000-8000-000000000002.json"),
        format!("{}\n", json!({"link":claude_link})),
    )
    .unwrap();
    let mut corrupted = events.clone();
    for event in &mut corrupted {
        event.origin.agent = Agent::Claude;
        match &mut event.body {
            Body::ToolCall(call) => call.call_id = "call_X_fc_Y".into(),
            Body::ToolResult(result) => result.call_id = "call_X_fc_Y".into(),
            _ => {}
        }
    }
    store(
        &home,
        "fork",
        &corrupted,
        json!({"created":"2026-01-01T00:00:00.000Z", "parent":{"session":"parent", "event":null}, "link":{"host":host,"agent":"claude","native_id":"claude-1","path":"/new/claude.jsonl","exported_events":3,"direction":"import"}}),
    );
    store(
        &home,
        "corrupted",
        &corrupted,
        json!({"created":"2026-01-01T00:00:00.000Z"}),
    );
    fs::write(home.path("baton/version"), "2\n").unwrap();
    let output = home.baton(&["migrate"]);
    assert!(output.contains("1 redundant forks merged"), "{output}");
    assert!(!home.session("fork").exists());
    let records: Vec<Value> = fs::read_dir(home.session("parent/records"))
        .unwrap()
        .map(|entry| serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|record| record["link"]["native_id"] == "claude-1"
                && record["link"]["path"] == "/new/claude.jsonl"
                && record["link"]["continued"] == false)
    );
    for session in ["parent", "corrupted"] {
        for entry in fs::read_dir(home.session(&format!("{session}/events"))).unwrap() {
            let bytes =
                zstd::stream::decode_all(fs::File::open(entry.unwrap().path()).unwrap()).unwrap();
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let event: Event = serde_json::from_slice(line).unwrap();
                let expected = Event::chain(
                    [draft(1, "entry", event.body.clone())],
                    event.parent.clone(),
                    event.origin.agent,
                    &event.origin.native_id,
                );
                assert_eq!(event.id, expected[0].id);
            }
        }
    }
    assert!(
        home.baton(&["migrate"])
            .contains("Nothing needed migrating")
    );
}

#[test]
fn legacy_export_backflow_does_not_fork() {
    let home = Home::new();
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
                json!({"role":"toolResult", "toolCallId":"call_X|fc_Y", "content":[{"type":"text", "text":"output"}], "isError":false}),
            ),
        ],
    );
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);
    let id = home.export(&session, "claude");
    let transcript = home.claude_transcript(&id);
    let text = fs::read_to_string(&transcript).unwrap();
    let hashed = baton_core::portable_call_id("call_X|fc_Y");
    assert!(text.contains(&hashed));
    fs::write(&transcript, text.replace(&hashed, "call_X_fc_Y")).unwrap();
    home.baton(&["import", "--agent", "claude"]);
    assert_eq!(home.sessions(&[]), vec![session]);
}

#[test]
fn materialize_removes_duplicate_current_exports() {
    let home = Home::new();
    write_pi(&home, &[header("/work"), ask("a1", None, 1, "question")]);
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);
    let first = home.export(&session, "claude");
    let second = home.export(&session, "claude");
    assert_ne!(first, second);
    assert_eq!(home.claude_transcripts().len(), 2);
    home.baton(&["materialize", "--in", "claude"]);
    let remaining = home.claude_transcripts();
    assert_eq!(remaining.len(), 1);
    let remaining_id = remaining[0].file_stem().unwrap().to_str().unwrap();
    assert!(remaining_id == first || remaining_id == second);
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
