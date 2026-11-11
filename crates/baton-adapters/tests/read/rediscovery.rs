//! Discovery tracks changes to native stores and reads the selected source

use crate::support;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
#[cfg(unix)]
use baton_adapters::OpencodeAdapter;
use baton_adapters::{
    AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter, DevinAdapter,
    HermesAdapter,
};
#[cfg(unix)]
use baton_core::Agent;
use baton_core::{Blobs, NativeSession, Source};
#[cfg(unix)]
use rusqlite::Connection;

fn text(adapter: &dyn Source, native: &NativeSession, blobs: &Blobs) -> Vec<String> {
    adapter
        .read(native, None, blobs)
        .unwrap()
        .drafts
        .iter()
        .map(|draft| draft.body.text().into_owned())
        .collect()
}

#[test]
fn hermes_detects_message_edits_and_changed_compression_links() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let connection = support::database(&db, include_str!("../fixtures/hermes-state.sql"));
    connection.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO sessions (id,source,started_at,end_reason) VALUES ('root','cli',1,'compression'); INSERT INTO sessions (id,source,parent_session_id,started_at) VALUES ('child','cli','root',2); INSERT INTO messages (session_id,role,content,timestamp) VALUES ('root','user','first',1),('child','user','second',2);").unwrap();
    let adapter = HermesAdapter::new(dir.path().to_path_buf());
    let blobs = Blobs::new(dir.path().join("blobs"));
    let before = support::only(&adapter, None);
    assert_eq!(text(&adapter, &before, &blobs), ["first", "second"]);
    connection
        .execute(
            "UPDATE messages SET content='edited' WHERE session_id='root'",
            [],
        )
        .unwrap();
    let edited = support::only(&adapter, None);
    assert_ne!(before.stamp, edited.stamp);
    assert_eq!(text(&adapter, &edited, &blobs), ["edited", "second"]);
    connection
        .execute("UPDATE sessions SET end_reason='stop' WHERE id='root'", [])
        .unwrap();
    let changed = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .find(|s| s.id == "root")
        .unwrap();
    assert_ne!(edited.stamp, changed.stamp);
    assert_eq!(text(&adapter, &changed, &blobs), ["edited"]);
}

#[test]
fn devin_detects_changed_main_chain_without_new_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let connection = support::database(&db, include_str!("../fixtures/devin-sessions.sql"));
    connection.execute_batch(r#"PRAGMA journal_mode=WAL;
INSERT INTO sessions (id,working_directory,backend_type,model,agent_mode,created_at,last_activity_at,main_chain_id) VALUES ('session','/work','windsurf','model','normal',1,1,1);
INSERT INTO message_nodes (session_id,node_id,parent_node_id,chat_message,created_at) VALUES ('session',1,NULL,'{"role":"assistant","content":"first"}',1),('session',2,NULL,'{"role":"assistant","content":"second"}',1);"#).unwrap();
    let adapter = DevinAdapter::new(db);
    let blobs = Blobs::new(dir.path().join("blobs"));
    let before = support::only(&adapter, None);
    assert_eq!(text(&adapter, &before, &blobs), ["first"]);
    connection
        .execute("UPDATE sessions SET main_chain_id=2 WHERE id='session'", [])
        .unwrap();
    let after = support::only(&adapter, None);
    assert_ne!(before.stamp, after.stamp);
    assert_eq!(text(&adapter, &after, &blobs), ["second"]);
}

fn field(number: u8, bytes: &[u8]) -> Vec<u8> {
    assert!(number < 16 && bytes.len() < 128);
    let mut output = vec![(number << 3) | 2, bytes.len() as u8];
    output.extend_from_slice(bytes);
    output
}

fn summary(id: &str, cwd: &str) -> Vec<u8> {
    let workspace = field(1, format!("file://{cwd}").as_bytes());
    let value = field(9, &workspace);
    let mut entry = field(1, id.as_bytes());
    entry.extend(field(2, &value));
    field(1, &entry)
}

#[test]
fn antigravity_cli_summary_change_updates_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let id = "12345678-1234-4234-8234-123456789abc";
    let db = root.join("conversations").join(format!("{id}.db"));
    support::database(
        &db,
        "CREATE TABLE trajectory_metadata_blob (id TEXT, data BLOB); INSERT INTO trajectory_metadata_blob VALUES ('main',X''); CREATE TABLE steps (idx INTEGER, status INTEGER, metadata BLOB, step_payload BLOB);",
    );
    let summaries = support::database(
        &root.join("conversation_summaries.db"),
        "CREATE TABLE conversation_summaries (conversation_id TEXT, title TEXT, workspace_uris TEXT, parent_conversation_id TEXT, nesting_depth INTEGER); INSERT INTO conversation_summaries VALUES ('12345678-1234-4234-8234-123456789abc','title','[\"file:///one\"]','',0); PRAGMA journal_mode=WAL;",
    );
    let adapter = AntigravityAdapter::new(root.to_path_buf());
    let blobs = Blobs::new(root.join("blobs"));
    let before = support::only(&adapter, None);
    assert_eq!(
        adapter
            .read(&before, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/one")
    );
    summaries
        .execute(
            "UPDATE conversation_summaries SET workspace_uris='[\"file:///two\"]'",
            [],
        )
        .unwrap();
    let after = support::only(&adapter, None);
    assert_ne!(before.stamp, after.stamp);
    assert_eq!(
        adapter
            .read(&after, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/two")
    );
}

#[test]
fn antigravity_desktop_summary_change_updates_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let id = "12345678-1234-4234-8234-123456789abc";
    let db = root.join("conversations").join(format!("{id}.db"));
    support::database(
        &db,
        &format!(
            "CREATE TABLE trajectory_meta (cascade_id TEXT); INSERT INTO trajectory_meta VALUES ('{id}'); CREATE TABLE trajectory_metadata_blob (id TEXT, data BLOB); INSERT INTO trajectory_metadata_blob VALUES ('main',X''); CREATE TABLE steps (idx INTEGER, status INTEGER, metadata BLOB, step_payload BLOB);"
        ),
    );
    let cache = root.join("agyhub_summaries_proto.pb");
    fs::write(&cache, summary(id, "/one")).unwrap();
    let adapter = AntigravityDesktopAdapter::new(root.to_path_buf());
    let blobs = Blobs::new(root.join("blobs"));
    let before = support::only(&adapter, None);
    assert_eq!(
        adapter
            .read(&before, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/one")
    );
    fs::write(&cache, summary(id, "/two-longer")).unwrap();
    let after = support::only(&adapter, None);
    assert_ne!(before.stamp, after.stamp);
    assert_eq!(
        adapter
            .read(&after, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/two-longer")
    );
}

#[test]
fn antigravity_ide_accepts_transcript_and_tracks_summary_database() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let id = "12345678-1234-4234-8234-123456789abc";
    let log = root
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl");
    support::write(&log, "{\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n");
    let db = root.join("editor/User/globalStorage/state.vscdb");
    let connection = support::database(
        &db,
        "CREATE TABLE ItemTable (key TEXT, value TEXT); PRAGMA journal_mode=WAL;",
    );
    let encode = |cwd: &str| {
        let workspace = field(1, format!("file://{cwd}").as_bytes());
        let value = field(9, &workspace);
        let wrapped = field(1, STANDARD.encode(value).as_bytes());
        let mut entry = field(1, id.as_bytes());
        entry.extend(field(2, &wrapped));
        STANDARD.encode(field(1, &entry))
    };
    connection
        .execute(
            "INSERT INTO ItemTable VALUES ('antigravityUnifiedStateSync.trajectorySummaries', ?1)",
            [encode("/one")],
        )
        .unwrap();
    let adapter = AntigravityIdeAdapter::new(root.to_path_buf(), root.join("editor"));
    let blobs = Blobs::new(root.join("blobs"));
    let before = support::only(&adapter, Some(&log));
    assert_eq!(before.path, log);
    assert_eq!(text(&adapter, &before, &blobs), ["hello"]);
    assert_eq!(
        adapter
            .read(&before, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/one")
    );
    connection
        .execute("UPDATE ItemTable SET value=?1", [encode("/two")])
        .unwrap();
    let after = support::only(&adapter, Some(&log));
    assert_ne!(before.stamp, after.stamp);
    assert_eq!(
        adapter
            .read(&after, None, &blobs)
            .unwrap()
            .info
            .cwd
            .as_deref(),
        Some("/two")
    );
    connection.execute("DROP TABLE ItemTable", []).unwrap();
    assert!(adapter.discover(Some(&log)).is_err());
}

#[cfg(unix)]
#[test]
fn opencode_and_kilo_export_the_selected_database_with_wal() {
    if let Some(root) = std::env::var_os("BATON_STAMP_TEST_ROOT") {
        let root = Path::new(&root);
        let blobs = Blobs::new(root.join("blobs"));
        for (agent, name) in [(Agent::Opencode, "opencode"), (Agent::Kilo, "kilo")] {
            let adapter = OpencodeAdapter::new(agent, root.join("wrong"));
            let native = support::only(
                &adapter,
                Some(&root.join(format!("{name}-alternate.sqlite"))),
            );
            assert_eq!(
                text(&adapter, &native, &blobs),
                [format!("{name} from wal")]
            );
            let adapter = OpencodeAdapter::new(agent, root.join("data").join(name));
            let native = support::only(&adapter, None);
            assert_eq!(
                text(&adapter, &native, &blobs),
                [format!("{name} direct from wal")]
            );
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let script = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf '1.0.0\n'
    exit 0
fi
export BATON_FAKE_NAME="${0##*/}"
export BATON_FAKE_ARGS="$*"
exec "$BATON_FAKE_TEST_EXE" --exact rediscovery::opencode_cli_fake --nocapture
"#;
    for name in ["opencode", "kilo"] {
        let binary = bin.join(name);
        fs::write(&binary, script).unwrap();
        fs::set_permissions(binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut connections = Vec::new();
    for name in ["opencode", "kilo"] {
        let db = root.join(format!("{name}-alternate.sqlite"));
        let connection = Connection::open(db).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session (id TEXT, title TEXT, time_updated INTEGER, parent_id TEXT); INSERT INTO session VALUES ('session','before',1,NULL);").unwrap();
        connection
            .execute("UPDATE session SET title=?1", [format!("{name} from wal")])
            .unwrap();
        connections.push(connection);
        let db = root.join("data").join(name).join(format!("{name}.db"));
        let connection = support::database(
            &db,
            "PRAGMA journal_mode=WAL; CREATE TABLE session (id TEXT, title TEXT, time_updated INTEGER, parent_id TEXT); INSERT INTO session VALUES ('session','before',1,NULL);",
        );
        connection
            .execute(
                "UPDATE session SET title=?1",
                [format!("{name} direct from wal")],
            )
            .unwrap();
        connections.push(connection);
    }
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    support::run_in_child(
        "rediscovery::opencode_and_kilo_export_the_selected_database_with_wal",
        None,
        |command| {
            command
                .env("BATON_STAMP_TEST_ROOT", root)
                .env("BATON_FAKE_TEST_EXE", std::env::current_exe().unwrap())
                .env("PATH", path);
        },
    );
    drop(connections);
}

#[cfg(unix)]
#[test]
fn opencode_cli_fake() {
    let Ok(name) = std::env::var("BATON_FAKE_NAME") else {
        return;
    };
    assert_eq!(
        std::env::var("BATON_FAKE_ARGS").unwrap(),
        "--pure export session"
    );
    let db = std::path::PathBuf::from(std::env::var_os("OPENCODE_DB").unwrap());
    assert_eq!(std::env::var_os("KILO_DB").unwrap(), db.as_os_str());
    // macOS temp directories use a /var symlink that the adapter resolves
    let db = db.canonicalize().unwrap();
    let root = Path::new(&std::env::var_os("BATON_STAMP_TEST_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    assert!(db.starts_with(&root));
    let direct = root.join("data").join(&name).join(format!("{name}.db"));
    if db == direct {
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME").unwrap(),
            root.join("data").as_os_str()
        );
    }
    let connection = Connection::open(db).unwrap();
    let title: String = connection
        .query_row(
            "SELECT title FROM session WHERE id = 'session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    println!(
        "{}",
        serde_json::json!({"info": {"directory": "/work"}, "messages": [{"info": {"id": "m1", "role": "user", "time": {"created": 1000}}, "parts": [{"type": "text", "text": title}]}]})
    );
}
