//! Every file based adapter reads back exactly what it wrote, which is what lets exported sessions flow back.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use baton_adapters::{ClaudeAdapter, CodexAdapter, PiAdapter};
use baton_core::{Adapter, Agent, Blobs, Body, Event, Export, Image, replay};
use serde_json::json;

const CREATED: &str = "2026-01-01T00:00:00.000Z";

fn sample(blobs: &Blobs) -> Vec<Event> {
    let image = Image {
        blob: blobs.put(b"pixels").unwrap(),
        mime: "image/png".into(),
    };
    let bodies = vec![
        support::user("first question", vec![]),
        support::user("look at this", vec![image.clone()]),
        support::thinking("private reasoning", Some("signature")),
        support::assistant("let me check"),
        support::call("call_1", "read", json!({"path": "a.txt"})),
        support::call("call_2", "bash", json!({"command": "ls", "env": {"A": 1}})),
        support::result("call_1", "file body", vec![image], false),
        support::result("call_2", "permission denied", vec![], true),
        support::assistant("done"),
        support::meta(json!({"type": "model", "model": "x"})),
        support::compaction("summary so far"),
        support::user("after compaction", vec![]),
        support::assistant("answer"),
    ];
    Event::chain(
        bodies.into_iter().map(support::draft),
        None,
        Agent::Pi,
        "source",
    )
}

struct Back {
    expected: Vec<Body>,
    read: Vec<Body>,
    title: Option<String>,
    cwd: Option<String>,
    file: PathBuf,
}

fn round_trip(adapter: &dyn Adapter, blobs: &Blobs) -> Back {
    let events = sample(blobs);
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Round trip"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
    };
    let written = adapter.write(&export, blobs).unwrap();
    let native = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .find(|native| native.id == written.id)
        .expect("a written session is discoverable");
    let read = adapter.read(&native, None, blobs).unwrap();
    Back {
        expected: replayed.iter().map(|event| event.body.clone()).collect(),
        read: support::bodies(&read.drafts),
        title: read.info.title.or(native.title),
        cwd: read.info.cwd,
        file: native.path,
    }
}

#[test]
fn pi_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(&PiAdapter::new(Agent::Pi, dir.path().join("pi")), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    assert!(
        dir.path().join("pi/sessions/--work-demo--").is_dir(),
        "pi buckets sessions by the escaped project path"
    );
}

#[test]
fn omp_round_trips_exactly_with_a_title_slot_and_blob_files() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("omp");
    let back = round_trip(&PiAdapter::new(Agent::Omp, root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    let text = fs::read_to_string(&back.file).unwrap();
    assert_eq!(
        text.find('\n'),
        Some(255),
        "the title slot is exactly 256 bytes"
    );
    assert!(
        text.contains("blob:sha256:"),
        "Oh My Pi references images by hash"
    );
    assert_eq!(fs::read_dir(root.join("blobs")).unwrap().count(), 1);
    assert!(
        PiAdapter::new(Agent::Pi, root)
            .discover(None)
            .unwrap()
            .is_empty(),
        "pi must not claim Oh My Pi journals"
    );
}

#[test]
fn claude_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("claude");
    let back = round_trip(&ClaudeAdapter::new(root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    assert!(
        root.join("projects/-work-demo").is_dir(),
        "Claude Code names project directories after the dashed path"
    );
}

#[test]
fn codex_round_trips_everything_but_tool_error_flags() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("codex");
    support::write(
        &root.join("config.toml"),
        "model = \"m\"\nmodel_provider = \"custom\" # local proxy\n\n[model_providers.custom]\nmodel_provider = \"nested\"\n",
    );
    let back = round_trip(&CodexAdapter::new(root.clone()), &blobs);
    let rollout = fs::read_to_string(&back.file).unwrap();
    assert!(
        rollout.contains(r#""model_provider":"custom""#),
        "Codex only resumes rollouts of the configured provider"
    );
    assert_eq!(replay::digest(&back.read), replay::digest(&back.expected));
    assert_eq!(back.read.len(), back.expected.len());
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let index = fs::read_to_string(root.join("session_index.jsonl")).unwrap();
    assert!(
        index.contains("Round trip"),
        "Codex keeps titles in its session index"
    );
}
