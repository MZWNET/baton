#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use baton_adapters::PiAdapter;
use baton_core::{
    Agent, Assistant, Blobs, Body, Compaction, Draft, Image, NativeRead, NativeSession, Source,
    Thinking, ToolCall, ToolResult, User,
};
use rusqlite::Connection;
use serde_json::{Value, json};

pub fn only(adapter: &dyn Source, path: Option<&Path>) -> NativeSession {
    let mut natives = adapter.discover(path).unwrap();
    assert_eq!(natives.len(), 1, "expected exactly one session");
    natives.remove(0)
}

pub fn read_session(adapter: &dyn Source, blobs: &Blobs) -> anyhow::Result<NativeRead> {
    adapter.read(&only(adapter, None), None, blobs)
}

pub fn run_in_child(test: &str, shell: Option<&str>, configure: impl FnOnce(&mut Command)) {
    let exe = std::env::current_exe().unwrap();
    let mut command = if let Some(shell) = shell {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{shell}; exec \"$@\""))
            .arg("sh")
            .arg(exe);
        command
    } else {
        Command::new(exe)
    };
    command.arg("--exact").arg(test);
    configure(&mut command);
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "child test {test} failed:\n{stdout}\n{stderr}"
    );
}

pub fn ids(bodies: &[Body]) -> std::collections::HashMap<&str, &str> {
    bodies
        .iter()
        .filter_map(|body| match body {
            Body::ToolCall(call) => Some((call.name.as_str(), call.call_id.as_str())),
            Body::ToolResult(result) => Some((result.text.as_str(), result.call_id.as_str())),
            _ => None,
        })
        .collect()
}

pub fn portable(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn pi_session(root: &Path, messages: &[Value]) -> PiAdapter {
    const CREATED: &str = "2026-01-01T00:00:00.000Z";
    let mut records = vec![
        json!({"type": "session", "version": 3, "id": "pi-session", "timestamp": CREATED, "cwd": "/workspace/demo"}),
    ];
    let mut parent: Option<String> = None;
    for (index, message) in messages.iter().enumerate() {
        let id = format!("m{index}");
        records.push(json!({"type": "message", "id": id, "parentId": parent, "timestamp": CREATED, "message": message}));
        parent = Some(id);
    }
    write(
        &root.join("sessions/--workspace-demo--/2026-01-01T00-00-00-000Z_pi-session.jsonl"),
        &jsonl(&records),
    );
    PiAdapter::new(Agent::Pi, root.to_path_buf())
}

pub fn jsonl(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

// A database at `path` holding `sql`, such as the schema an agent creates
pub fn database(path: &Path, sql: &str) -> Connection {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(sql).unwrap();
    connection
}

// A RikkaHub backup as the app exports it: `entries`, then its database with the conversations `fill` inserts
pub fn rikkahub_backup(path: &Path, fill: impl FnOnce(&Connection), entries: &[(&str, &[u8])]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rikka_hub.db");
    let connection = database(&db, include_str!("../fixtures/rikkahub-rikka_hub.sql"));
    fill(&connection);
    connection.close().unwrap();

    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.start_file("rikka_hub.db", options).unwrap();
    zip.write_all(&fs::read(db).unwrap()).unwrap();
    zip.finish().unwrap();
}

// Every entry of a ZIP with its bytes, in archive order
pub fn zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut zip = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    (0..zip.len())
        .map(|index| {
            let mut entry = zip.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

pub fn draft(body: Body) -> Draft {
    Draft {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        native_entry_id: None,
        body,
    }
}

pub fn bodies(drafts: &[Draft]) -> Vec<Body> {
    drafts.iter().map(|draft| draft.body.clone()).collect()
}

pub fn user(text: &str, images: Vec<Image>) -> Body {
    Body::User(User {
        text: text.into(),
        images,
    })
}

pub fn assistant(text: &str) -> Body {
    Body::Assistant(Assistant { text: text.into() })
}

pub fn thinking(text: &str, signature: Option<&str>) -> Body {
    Body::Thinking(Thinking {
        text: text.into(),
        signature: signature.map(String::from),
    })
}

pub fn call(id: &str, name: &str, input: Value) -> Body {
    Body::ToolCall(ToolCall {
        call_id: id.into(),
        name: name.into(),
        input,
    })
}

pub fn result(id: &str, text: &str, images: Vec<Image>, is_error: bool) -> Body {
    Body::ToolResult(ToolResult {
        call_id: id.into(),
        text: text.into(),
        images,
        is_error,
    })
}

pub fn compaction(summary: &str) -> Body {
    Body::Compaction(Compaction {
        summary: summary.into(),
    })
}

pub fn meta(value: Value) -> Body {
    Body::Meta(value)
}
