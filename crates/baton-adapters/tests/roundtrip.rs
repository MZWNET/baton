//! Every file based adapter reads back exactly what it wrote, which is what lets exported sessions flow back.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use baton_adapters::{
    AntigravityAdapter, ClaudeAdapter, CodexAdapter, CopilotAdapter, CursorAdapter, DevinAdapter,
    GrokAdapter, HermesAdapter, KimiAdapter, MastracodeAdapter, MuseAdapter, OpenhandsAdapter,
    PiAdapter, QwenAdapter, VibeAdapter,
};
use baton_core::{Adapter, Agent, Blobs, Body, Event, Export, Image, Source, replay};
use rusqlite::Connection;
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

#[test]
fn hermes_round_trips_everything_but_tool_error_flags_and_replays_from_the_last_summary() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let home = dir.path().join("hermes");
    let db = home.join("state.db");
    support::database(&db, include_str!("fixtures/hermes-state.sql"));
    let adapter = HermesAdapter::new(home);
    let back = round_trip(&adapter, &blobs);
    assert_eq!(replay::digest(&back.read), replay::digest(&back.expected));
    assert_eq!(back.read.len(), back.expected.len());
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let connection = Connection::open(&db).unwrap();
    let mut statement = connection
        .prepare("SELECT content FROM messages WHERE active = 1 ORDER BY id")
        .unwrap();
    let replayed: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        replayed,
        [
            "[CONTEXT SUMMARY]:\nsummary so far",
            "after compaction",
            "answer"
        ],
        "Hermes replays the summary instead of the history it replaced"
    );
}

#[test]
fn hermes_leaves_out_a_title_another_session_holds() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let home = dir.path().join("hermes");
    support::database(
        &home.join("state.db"),
        include_str!("fixtures/hermes-state.sql"),
    );
    let adapter = HermesAdapter::new(home);
    round_trip(&adapter, &blobs);
    let again = round_trip(&adapter, &blobs);
    assert_eq!(replay::digest(&again.read), replay::digest(&again.expected));
    assert_eq!(
        again.title, None,
        "Hermes titles are unique, and a numbered one would rename the baton session"
    );
}

#[test]
fn mastracode_round_trips_exactly_into_a_database_it_creates() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("mastracode/mastra.db");
    let back = round_trip(&MastracodeAdapter::new(db.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let connection = Connection::open(&db).unwrap();
    let resource: String = connection
        .query_row("SELECT \"resourceId\" FROM mastra_threads", [], |row| {
            row.get(0)
        })
        .unwrap();
    if let Some(override_id) =
        std::env::var_os("MASTRA_RESOURCE_ID").filter(|value| !value.is_empty())
    {
        assert_eq!(resource, override_id.to_string_lossy());
    } else {
        assert!(
            resource.starts_with("demo-") && resource.len() == "demo-".len() + 12,
            "MastraCode files threads under the project's slug and path hash: {resource}"
        );
    }
}

#[test]
fn devin_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("devin/sessions.db");
    support::database(&db, include_str!("fixtures/devin-sessions.sql"));
    let back = round_trip(&DevinAdapter::new(db.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let connection = Connection::open(&db).unwrap();
    let searchable: i64 = connection
        .query_row("SELECT count(*) FROM message_search", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        searchable, 6,
        "Devin's search indexes typed user input and replies, not summaries or tool output"
    );
}

#[test]
fn copilot_round_trips_exactly_with_the_title_in_its_workspace_file() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(&CopilotAdapter::new(dir.path().join("copilot")), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let workspace = fs::read_to_string(back.file.with_file_name("workspace.yaml")).unwrap();
    assert!(
        workspace.contains("name: \"Round trip\"\nuser_named: true\n"),
        "{workspace}"
    );
}

#[test]
fn grok_round_trips_exactly_under_its_encoded_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("grok");
    let back = round_trip(&GrokAdapter::new(root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    assert!(
        root.join("sessions/%2Fwork%2Fdemo").is_dir(),
        "Grok buckets sessions by the percent-encoded working directory"
    );
}

#[test]
fn openhands_round_trips_exactly_without_a_title() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(
        &OpenhandsAdapter::new(dir.path().join("conversations")),
        &blobs,
    );
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title, None, "OpenHands stores no title");
    let first = fs::read_dir(back.file.join("events"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .min()
        .unwrap();
    let first = fs::read_to_string(back.file.join("events").join(first)).unwrap();
    assert!(
        first.contains(r#""kind":"SystemPromptEvent""#),
        "OpenHands refuses a conversation that does not start with its system prompt"
    );
    assert!(
        !first.contains("+00:00") && !first.contains("Z\""),
        "OpenHands compares naive local timestamps: {first}"
    );
}

#[test]
fn vibe_round_trips_exactly_with_its_message_count() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(&VibeAdapter::new(dir.path().join("vibe")), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let lines = fs::read_to_string(&back.file).unwrap().lines().count();
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(back.file.with_file_name("meta.json")).unwrap())
            .unwrap();
    assert_eq!(
        meta["total_messages"], lines,
        "Vibe fails to save a session whose message count is off"
    );
}

#[test]
fn vibe_answers_calls_whose_results_come_later_with_lines_it_reads_past() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let bodies = vec![
        support::user("question", vec![]),
        support::assistant("first"),
        support::call("call_x", "read", json!({"path": "x"})),
        support::assistant("second"),
        support::call("call_y", "read", json!({"path": "y"})),
        support::result("call_x", "x body", vec![], false),
        support::result("call_y", "y body", vec![], true),
        support::result("orphan", "no call", vec![], false),
        support::user("next", vec![]),
        support::call("call_z", "read", json!({"path": "z"})),
    ];
    let events = Event::chain(
        bodies.into_iter().map(support::draft),
        None,
        Agent::Pi,
        "source",
    );
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: None,
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
    };
    let adapter = VibeAdapter::new(dir.path().join("vibe"));
    let written = adapter.write(&export, &blobs).unwrap();
    let read = adapter.read(&written, None, &blobs).unwrap();
    let expected: Vec<Body> = replayed.iter().map(|event| event.body.clone()).collect();
    assert_eq!(support::bodies(&read.drafts), expected);
}

#[test]
fn qwen_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("qwen");
    let back = round_trip(&QwenAdapter::new(root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    assert!(
        root.join("projects/-work-demo/chats").is_dir(),
        "Qwen Code names project directories after the dashed path"
    );
}

#[test]
fn kimi_round_trips_exactly_and_resumes_from_the_last_summary() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(&KimiAdapter::new(dir.path().join("kimi")), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let wire = fs::read_to_string(&back.file).unwrap();
    assert!(
        wire.contains(r#""compactedCount":6"#),
        "the summary replaces the six messages before it in Kimi's context: {wire}"
    );
}

#[test]
fn muse_round_trips_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(&MuseAdapter::new(dir.path().join("muse")), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let log = fs::read_to_string(&back.file).unwrap();
    let sequences: Vec<u64> = log
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["sequence"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(
        sequences,
        (1..=sequences.len() as u64).collect::<Vec<_>>(),
        "Muse refuses logs with gaps in their sequence"
    );
}

#[test]
fn antigravity_round_trips_exactly_and_lists_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("antigravity-cli");
    let back = round_trip(&AntigravityAdapter::new(root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    let summaries = Connection::open(root.join("conversation_summaries.db")).unwrap();
    let (steps, uris): (i64, String) = summaries
        .query_row(
            "SELECT step_count, workspace_uris FROM conversation_summaries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(steps, 9, "agy lists a conversation with its step count");
    assert_eq!(uris, r#"["file:///work/demo"]"#);
}

#[test]
fn cursor_round_trips_exactly_into_the_bucket_of_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("cursor");
    let back = round_trip(&CursorAdapter::new(root.clone()), &blobs);
    assert_eq!(back.read, back.expected);
    assert_eq!(back.title.as_deref(), Some("Round trip"));
    assert_eq!(back.cwd.as_deref(), Some("/work/demo"));
    assert_eq!(
        back.file.parent().and_then(|dir| dir.parent()),
        Some(
            root.join("chats/11dd9429831578384393182ae89b24ff")
                .as_path()
        ),
        "Cursor Agent files chats under the MD5 of their directory"
    );
}
