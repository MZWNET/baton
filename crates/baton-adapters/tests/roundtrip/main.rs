//! Every adapter baton writes reads back what it wrote, except what its format cannot keep

mod publication;
#[path = "../support/mod.rs"]
mod support;
mod vibe_continuation;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use baton_adapters::{
    AntigravityAdapter, ClaudeAdapter, CodexAdapter, CopilotAdapter, CursorAdapter, DevinAdapter,
    DshAdapter, GrokAdapter, HermesAdapter, KimiAdapter, MastracodeAdapter, MuseAdapter,
    OpenhandsAdapter, PiAdapter, QwenAdapter, RikkahubAdapter, VibeAdapter,
};
use baton_core::{Adapter, Agent, Blobs, Body, Event, Export, Image, Source, replay};
use rusqlite::Connection;
use serde_json::json;

const CREATED: &str = "2026-01-01T00:00:00.000Z";
const SETTINGS: &[u8] =
    br#"{"assistantId":"5f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b","launchCount":3}"#;

fn writable_adapter(agent: Agent, root: &Path) -> Option<Box<dyn Adapter>> {
    Some(match agent {
        Agent::Pi | Agent::Omp => Box::new(PiAdapter::new(agent, root.to_path_buf())),
        Agent::Claude => Box::new(ClaudeAdapter::new(root.to_path_buf())),
        Agent::Codex => Box::new(CodexAdapter::new(root.to_path_buf())),
        Agent::Mastracode => Box::new(MastracodeAdapter::new(root.join("mastra.db"))),
        // Hermes and Devin only write into a database their agent created
        Agent::Hermes => {
            if !root.join("state.db").exists() {
                support::database(
                    &root.join("state.db"),
                    include_str!("../fixtures/hermes-state.sql"),
                );
            }
            Box::new(HermesAdapter::new(root.to_path_buf()))
        }
        Agent::Devin => {
            if !root.join("sessions.db").exists() {
                support::database(
                    &root.join("sessions.db"),
                    include_str!("../fixtures/devin-sessions.sql"),
                );
            }
            Box::new(DevinAdapter::new(root.join("sessions.db")))
        }
        Agent::Copilot => Box::new(CopilotAdapter::new(root.to_path_buf())),
        Agent::Grok => Box::new(GrokAdapter::new(root.to_path_buf())),
        Agent::Openhands => Box::new(OpenhandsAdapter::new(root.to_path_buf())),
        Agent::Vibe => Box::new(VibeAdapter::new(root.to_path_buf())),
        Agent::Qwen => Box::new(QwenAdapter::new(root.to_path_buf())),
        Agent::Kimi => Box::new(KimiAdapter::new(root.to_path_buf())),
        Agent::Dsh => Box::new(DshAdapter::new(root.to_path_buf())),
        Agent::Muse => Box::new(MuseAdapter::new(root.to_path_buf())),
        Agent::Antigravity => Box::new(AntigravityAdapter::new(root.to_path_buf())),
        Agent::Cursor => Box::new(CursorAdapter::new(root.to_path_buf())),
        Agent::Rikkahub => {
            fs::create_dir_all(root).unwrap();
            Box::new(RikkahubAdapter::new(Some(root.join("backup.zip"))))
        }
        // Tests cannot assume their CLIs are installed; src/opencode/tests.rs round-trips their formats
        Agent::Opencode | Agent::Kilo => return None,
        // Baton only reads these agents
        Agent::AntigravityDesktop | Agent::AntigravityIde | Agent::CursorIde | Agent::Paseo => {
            return None;
        }
    })
}

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
        support::result("call_2", "permission denied", vec![], true),
        support::result("call_1", "file body", vec![image], false),
        support::result("orphan", "isolated result", vec![], false),
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

fn round_trip(agent: Agent, root: &Path, blobs: &Blobs) -> Back {
    let events = sample(blobs);
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Round trip"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
        seed: None,
    };
    let adapter = writable_adapter(agent, root).unwrap();
    let written = adapter.write(&export, blobs).unwrap();
    let before_flush = adapter
        .read(&written, None, blobs)
        .unwrap_or_else(|error| panic!("{agent} read before flush failed: {error:#}"));
    adapter.flush().unwrap();
    drop(adapter);
    let adapter = writable_adapter(agent, root).unwrap();
    let native = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .find(|native| native.id == written.id)
        .expect("a written session is discoverable");
    let read = adapter.read(&native, None, blobs).unwrap();
    assert_eq!(
        support::bodies(&before_flush.drafts),
        support::bodies(&read.drafts),
        "{agent}"
    );

    Back {
        expected: replayed.iter().map(|event| event.body.clone()).collect(),
        read: support::bodies(&read.drafts),
        title: read.info.title.or(native.title),
        cwd: read.info.cwd,
        file: native.path,
    }
}

#[test]
fn seeded_exports_have_the_same_native_id_and_path_in_fresh_stores() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let events = sample(&blobs);
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Round trip"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
        seed: Some(uuid::Uuid::parse_str("12345678-1234-5678-9234-567812345678").unwrap()),
    };
    // RikkaHub canonicalizes its backup path, and macOS temp directories sit behind a /var symlink
    let root = dir.path().canonicalize().unwrap();
    let first_root = root.join("first");
    let second_root = root.join("second");
    for agent in Agent::ALL {
        let Some(first) = writable_adapter(agent, &first_root.join(agent.name())) else {
            continue;
        };
        let second = writable_adapter(agent, &second_root.join(agent.name())).unwrap();
        let first = first.write(&export, &blobs).unwrap();
        let second = second.write(&export, &blobs).unwrap();
        assert_eq!(first.id, second.id, "{agent}");
        assert_eq!(
            first.path.strip_prefix(&first_root).unwrap(),
            second.path.strip_prefix(&second_root).unwrap(),
            "{agent}"
        );
    }
}

#[test]
fn ids_that_differ_natively_keep_their_own_results_through_an_export() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let pi = support::pi_session(
        &dir.path().join("pi"),
        &[
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": [
                {"type": "toolCall", "id": "a|b", "name": "read", "arguments": {}},
                {"type": "toolCall", "id": "a_b", "name": "bash", "arguments": {}},
                {"type": "toolCall", "id": "a.b", "name": "grep", "arguments": {}},
            ]}),
            json!({"role": "toolResult", "toolCallId": "a_b", "content": "from bash"}),
            json!({"role": "toolResult", "toolCallId": "a.b", "content": "from grep"}),
            json!({"role": "toolResult", "toolCallId": "a|b", "content": "from read"}),
        ],
    );
    let read = support::read_session(&pi, &blobs).unwrap();
    let bodies = support::bodies(&read.drafts);
    let ids = support::ids(&bodies);
    assert_eq!(ids["bash"], "a_b");
    assert_eq!(ids["from bash"], ids["bash"]);
    assert_eq!(ids["from grep"], ids["grep"]);
    assert_eq!(ids["from read"], ids["read"]);
    assert_ne!(ids["read"], ids["bash"]);
    assert_ne!(ids["grep"], ids["bash"]);
    assert_ne!(ids["read"], ids["grep"]);
    assert!(ids.values().all(|id| support::portable(id)), "{ids:?}");

    let events = Event::chain(read.drafts, None, Agent::Pi, "pi-session");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: None,
        cwd: Path::new("/workspace/demo"),
        created: CREATED,
        events: &replayed,
        seed: None,
    };
    let claude = ClaudeAdapter::new(dir.path().join("claude"));
    claude.write(&export, &blobs).unwrap();
    let back = support::read_session(&claude, &blobs).unwrap();
    assert_eq!(support::bodies(&back.drafts), bodies);
}

#[test]
fn every_writable_agent_reads_back_what_it_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    for agent in Agent::ALL {
        let root = dir.path().join(agent.name());
        let Some(adapter) = writable_adapter(agent, &root) else {
            continue;
        };
        drop(adapter);
        let back = round_trip(agent, &root, &blobs);
        let keeps_error_flags = !matches!(agent, Agent::Codex | Agent::Hermes);
        let keeps_title = !matches!(agent, Agent::Openhands | Agent::Muse);
        let keeps_cwd = !matches!(agent, Agent::Openhands | Agent::Rikkahub);
        if keeps_error_flags {
            assert_eq!(back.read, back.expected, "{agent}");
        } else {
            assert_eq!(
                replay::digest(&back.read),
                replay::digest(&back.expected),
                "{agent}"
            );
            assert_eq!(back.read.len(), back.expected.len(), "{agent}");
        }
        assert_eq!(
            back.title.as_deref(),
            keeps_title.then_some("Round trip"),
            "{agent}"
        );
        assert_eq!(
            back.cwd.as_deref(),
            keeps_cwd.then_some("/work/demo"),
            "{agent}"
        );
    }
}

#[test]
fn rikkahub_adds_sessions_to_a_backup_and_keeps_the_rest_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let backup = dir.path().join("backup.zip");
    support::rikkahub_backup(
        &backup,
        |connection| {
            connection.execute(
                "INSERT INTO ConversationEntity (id, title, nodes, create_at, update_at) VALUES ('00000000-0000-4000-8000-000000000001', 'Earlier', '[]', 1, 2)",
                [],
            ).unwrap();
            connection.execute(
                "INSERT INTO memory_consolidation (conversation_id, consolidated_at) VALUES ('00000000-0000-4000-8000-000000000001', 3)",
                [],
            ).unwrap();
        },
        &[
            ("settings.json", SETTINGS),
            ("upload/kept.png", b"kept"),
            ("fonts/font.ttf", b"font"),
        ],
    );
    let back = round_trip(Agent::Rikkahub, dir.path(), &blobs);

    let entries = support::zip_entries(&backup);
    let names: HashSet<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names.len(),
        entries.len(),
        "RikkaHub refuses backups with duplicate entries"
    );
    assert_eq!(
        entries.len(),
        5,
        "the sample attaches one image twice, which the backup stores once"
    );
    let blob = blobs.put(b"pixels").unwrap();
    let upload = format!("upload/{blob}.png");
    for (name, expected) in [
        ("settings.json", SETTINGS),
        ("upload/kept.png", b"kept".as_slice()),
        ("fonts/font.ttf", b"font".as_slice()),
        (upload.as_str(), b"pixels".as_slice()),
    ] {
        assert_eq!(
            entries
                .iter()
                .find(|(entry, _)| entry == name)
                .unwrap()
                .1
                .as_slice(),
            expected
        );
    }
    let database = packed_database(dir.path(), &entries);
    let assistant_id: String = database
        .query_row(
            "SELECT assistant_id FROM ConversationEntity WHERE title = 'Round trip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        assistant_id, "5f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b",
        "RikkaHub lists conversations under the assistant selected in its settings"
    );
    let earlier: (String, i64) = database.query_row(
        "SELECT title, consolidated_at FROM ConversationEntity JOIN memory_consolidation ON id = conversation_id WHERE id = '00000000-0000-4000-8000-000000000001'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(earlier, ("Earlier".to_owned(), 3));
    assert_eq!(
        replay::digest(&reread(backup, &blobs)),
        replay::digest(&back.expected)
    );
}

#[test]
fn rikkahub_removes_a_conversation_without_deleting_a_shared_upload() {
    let dir = tempfile::tempdir().unwrap();
    let backup = dir.path().join("backup.zip");
    support::rikkahub_backup(&backup, |_| {}, &[]);
    let blobs = Blobs::new(dir.path().join("blobs"));
    let image = Image {
        blob: blobs.put(b"shared pixels").unwrap(),
        mime: "image/png".into(),
    };
    let body = support::user("shared image", vec![image.clone()]);
    let events = Event::chain([support::draft(body.clone())], None, Agent::Pi, "source");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Shared image"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
        seed: None,
    };
    let adapter = RikkahubAdapter::new(Some(backup.clone()));
    let removed = adapter.write(&export, &blobs).unwrap();
    let kept = adapter.write(&export, &blobs).unwrap();
    adapter.flush().unwrap();
    adapter.remove(&removed).unwrap();
    adapter.flush().unwrap();
    drop(adapter);

    let adapter = RikkahubAdapter::new(Some(backup.clone()));
    let sessions = adapter.discover(None).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, kept.id);
    let restored_blobs = Blobs::new(dir.path().join("restored-blobs"));
    let read = adapter.read(&sessions[0], None, &restored_blobs).unwrap();
    assert_eq!(support::bodies(&read.drafts), vec![body]);
    assert_eq!(restored_blobs.get(&image.blob).unwrap(), b"shared pixels");
    let upload = format!("upload/{}.png", image.blob);
    let entries = support::zip_entries(&backup);
    assert!(
        entries
            .iter()
            .any(|(name, bytes)| name == &upload && bytes == b"shared pixels")
    );
}

#[test]
fn rikkahub_creates_a_missing_backup_that_restores_without_touching_the_settings() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let backup = dir.path().join("backup.zip");
    let back = round_trip(Agent::Rikkahub, dir.path(), &blobs);

    let entries = support::zip_entries(&backup);
    let names: HashSet<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    let upload = format!("upload/{}.png", blobs.put(b"pixels").unwrap());
    assert_eq!(
        names,
        HashSet::from(["rikka_hub.db", upload.as_str()]),
        "restoring a backup without settings.json keeps the app's settings"
    );
    let database = packed_database(dir.path(), &entries);
    let version: i64 = database
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let identity: String = database
        .query_row(
            "SELECT identity_hash FROM room_master_table WHERE id = 42",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        (version, identity.as_str()),
        (32, "c64deeb13766eaf6528a2f4f6965c836"),
        "the app only opens a database whose schema version it can identify"
    );
    let assistant_id: String = database
        .query_row("SELECT assistant_id FROM ConversationEntity", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        assistant_id, "0950e2dc-9bd5-4801-afa3-aa887aa36b4e",
        "without settings, the sessions go to the app's built-in assistant"
    );
    assert_eq!(
        replay::digest(&reread(backup, &blobs)),
        replay::digest(&back.expected)
    );
}

// The database packed in a backup, opened from a copy
fn packed_database(dir: &Path, entries: &[(String, Vec<u8>)]) -> Connection {
    let (_, bytes) = entries
        .iter()
        .find(|(name, _)| name == "rikka_hub.db")
        .unwrap();
    let db = dir.join("packed.db");
    fs::write(&db, bytes).unwrap();
    Connection::open(db).unwrap()
}

// The one conversation of a packed backup, as a new adapter reads it
fn reread(backup: PathBuf, blobs: &Blobs) -> Vec<Body> {
    let adapter = RikkahubAdapter::new(Some(backup));
    let natives = adapter.discover(None).unwrap();
    let native = natives
        .iter()
        .find(|native| native.title.as_deref() == Some("Round trip"))
        .unwrap();
    support::bodies(&adapter.read(native, None, blobs).unwrap().drafts)
}

#[test]
fn pi_buckets_sessions_by_the_escaped_project_path() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    round_trip(Agent::Pi, &dir.path().join("pi"), &blobs);
    assert!(
        dir.path().join("pi/sessions/--work-demo--").is_dir(),
        "pi buckets sessions by the escaped project path"
    );
}

#[test]
fn omp_writes_a_title_slot_and_blob_files_without_pi_claiming_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("omp");
    let back = round_trip(Agent::Omp, &root, &blobs);
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
fn claude_names_project_directories_after_the_dashed_path() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("claude");
    round_trip(Agent::Claude, &root, &blobs);
    assert!(
        root.join("projects/-work-demo").is_dir(),
        "Claude Code names project directories after the dashed path"
    );
}

#[test]
fn codex_writes_the_configured_provider_and_indexes_the_title() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("codex");
    support::write(
        &root.join("config.toml"),
        "model = \"m\"\nmodel_provider = \"custom\" # local proxy\n\n[model_providers.custom]\nmodel_provider = \"nested\"\n",
    );
    let back = round_trip(Agent::Codex, &root, &blobs);
    let rollout = fs::read_to_string(&back.file).unwrap();
    assert!(
        rollout.contains(r#""model_provider":"custom""#),
        "Codex only resumes rollouts of the configured provider"
    );
    let index = fs::read_to_string(root.join("session_index.jsonl")).unwrap();
    assert!(
        index.contains("Round trip"),
        "Codex keeps titles in its session index"
    );
}

#[test]
fn codex_recreates_a_lost_seeded_rollout_without_reindexing_its_title() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let events = sample(&blobs);
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("Round trip"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
        seed: Some(uuid::Uuid::parse_str("12345678-1234-5678-9234-567812345678").unwrap()),
    };
    let root = dir.path().join("codex");
    let adapter = CodexAdapter::new(root.clone());
    let first = adapter.write(&export, &blobs).unwrap();
    fs::remove_file(&first.path).unwrap();
    let second = adapter.write(&export, &blobs).unwrap();
    assert_eq!(second.id, first.id);
    let index = fs::read_to_string(root.join("session_index.jsonl")).unwrap();
    assert_eq!(index.lines().count(), 1);
}

#[test]
fn hermes_replays_from_the_last_summary() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let home = dir.path().join("hermes");
    let db = home.join("state.db");
    support::database(&db, include_str!("../fixtures/hermes-state.sql"));
    round_trip(Agent::Hermes, &home, &blobs);
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
        include_str!("../fixtures/hermes-state.sql"),
    );
    round_trip(Agent::Hermes, &home, &blobs);
    let again = round_trip(Agent::Hermes, &home, &blobs);
    assert_eq!(replay::digest(&again.read), replay::digest(&again.expected));
    assert_eq!(
        again.title, None,
        "Hermes titles are unique, and a numbered one would rename the baton session"
    );
}

#[test]
fn mastracode_files_threads_under_the_project_resource_id() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("mastracode/mastra.db");
    round_trip(Agent::Mastracode, db.parent().unwrap(), &blobs);
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
fn devin_indexes_user_input_and_replies_for_search() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let db = dir.path().join("devin/sessions.db");
    support::database(&db, include_str!("../fixtures/devin-sessions.sql"));
    round_trip(Agent::Devin, db.parent().unwrap(), &blobs);
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
fn copilot_writes_the_title_in_its_workspace_file() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(Agent::Copilot, &dir.path().join("copilot"), &blobs);
    let workspace = fs::read_to_string(back.file.with_file_name("workspace.yaml")).unwrap();
    assert!(
        workspace.contains("name: \"Round trip\"\nuser_named: true\n"),
        "{workspace}"
    );
}

#[test]
fn grok_preserves_orphan_results_and_later_calls() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let image = Image {
        blob: blobs.put(b"pixels").unwrap(),
        mime: "image/png".into(),
    };
    let bodies = vec![
        support::user("look", vec![image.clone()]),
        support::result("missing", "orphan", vec![image.clone()], true),
        support::result("late", "before call", vec![], false),
        support::call("late", "read", json!({"path": "a"})),
        support::result("late", "after call", vec![], false),
    ];
    let events = Event::chain(
        bodies.clone().into_iter().map(support::draft),
        None,
        Agent::Pi,
        "source",
    );
    let path: Vec<&Event> = events.iter().collect();
    let adapter = GrokAdapter::new(dir.path().join("grok"));
    let native = adapter
        .write(
            &Export {
                title: None,
                cwd: Path::new("/work"),
                created: CREATED,
                events: &path,
                seed: None,
            },
            &blobs,
        )
        .unwrap();
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(support::bodies(&read.drafts), bodies);
}

#[test]
fn grok_buckets_sessions_by_the_encoded_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("grok");
    round_trip(Agent::Grok, &root, &blobs);
    assert!(
        root.join("sessions/%2Fwork%2Fdemo").is_dir(),
        "Grok buckets sessions by the percent-encoded working directory"
    );
}

#[test]
fn openhands_writes_a_system_prompt_and_naive_local_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(Agent::Openhands, &dir.path().join("conversations"), &blobs);
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
fn openhands_preserves_timestamps_in_the_repeated_dst_hour() {
    const CHILD: &str = "BATON_OPENHANDS_DST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        support::run_in_child(
            "openhands_preserves_timestamps_in_the_repeated_dst_hour",
            None,
            |command| {
                command.env(CHILD, "1").env("TZ", "EST5EDT,M3.2.0,M11.1.0");
            },
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let bodies = vec![support::user("first", vec![]), support::assistant("second")];
    let timestamps = ["2026-11-01T05:30:00.000Z", "2026-11-01T06:10:00.000Z"];
    let drafts = bodies
        .iter()
        .cloned()
        .zip(timestamps)
        .map(|(body, timestamp)| {
            let mut draft = support::draft(body);
            draft.timestamp = timestamp.into();

            draft
        });
    let events = Event::chain(drafts, None, Agent::Pi, "source");
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let adapter = OpenhandsAdapter::new(dir.path().join("conversations"));
    let native = adapter
        .write(
            &Export {
                title: None,
                cwd: Path::new("/work"),
                created: timestamps[0],
                events: &replayed,
                seed: None,
            },
            &blobs,
        )
        .unwrap();
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(support::bodies(&read.drafts), bodies);
    assert_eq!(
        read.drafts
            .iter()
            .map(|draft| draft.timestamp.as_str())
            .collect::<Vec<_>>(),
        timestamps
    );
}

#[test]
fn vibe_writes_the_message_count_in_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(Agent::Vibe, &dir.path().join("vibe"), &blobs);
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
        support::result("call_y", "y body", vec![], true),
        support::result("call_x", "x body", vec![], false),
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
        seed: None,
    };
    let root = dir.path().join("vibe");
    let adapter = VibeAdapter::new(root.clone());
    let written = adapter.write(&export, &blobs).unwrap();
    adapter.flush().unwrap();
    drop(adapter);
    let adapter = VibeAdapter::new(root);
    let native = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .find(|native| native.id == written.id)
        .unwrap();
    let read = adapter.read(&native, None, &blobs).unwrap();
    let expected: Vec<Body> = replayed.iter().map(|event| event.body.clone()).collect();
    assert_eq!(support::bodies(&read.drafts), expected);
}

#[test]
fn qwen_names_project_directories_after_the_dashed_path() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("qwen");
    round_trip(Agent::Qwen, &root, &blobs);
    assert!(
        root.join("projects/-work-demo/chats").is_dir(),
        "Qwen Code names project directories after the dashed path"
    );
}

#[test]
fn kimi_records_the_count_replaced_by_the_last_summary() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(Agent::Kimi, &dir.path().join("kimi"), &blobs);
    let wire = fs::read_to_string(&back.file).unwrap();
    assert!(
        wire.contains(r#""compactedCount":7"#),
        "the summary replaces the seven messages before it in Kimi's context: {wire}"
    );
}

#[test]
fn dsh_writes_its_session_file_in_the_project_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("dsh");
    let back = round_trip(Agent::Dsh, &root, &blobs);
    assert_eq!(back.file.file_name().unwrap(), "session.v4.jsonl.zstd");
    assert_eq!(
        back.file.parent().and_then(|dir| dir.parent()),
        Some(root.join("sessions/--work-demo--").as_path())
    );
}

#[test]
fn muse_writes_gapless_sequence_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let back = round_trip(Agent::Muse, &dir.path().join("muse"), &blobs);
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
fn antigravity_lists_the_conversation_in_its_summary_database() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("antigravity-cli");
    round_trip(Agent::Antigravity, &root, &blobs);
    let summaries = Connection::open(root.join("conversation_summaries.db")).unwrap();
    let (steps, uris): (i64, String) = summaries
        .query_row(
            "SELECT step_count, workspace_uris FROM conversation_summaries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(steps, 10, "agy lists a conversation with its step count");
    assert_eq!(uris, r#"["file:///work/demo"]"#);
}

#[test]
fn cursor_buckets_chats_by_the_md5_of_their_directory() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let root = dir.path().join("cursor");
    let back = round_trip(Agent::Cursor, &root, &blobs);
    assert_eq!(
        back.file.parent().and_then(|dir| dir.parent()),
        Some(
            root.join("chats/11dd9429831578384393182ae89b24ff")
                .as_path()
        ),
        "Cursor Agent files chats under the MD5 of their directory"
    );
}
