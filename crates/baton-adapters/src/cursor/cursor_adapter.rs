use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source, time,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::cursor::{graph_parser, graph_writer};
use crate::support::{files, identifiers, json, program, sqlite};

// The tables of a store, as Cursor Agent creates them
const SCHEMA: &str = "
PRAGMA user_version = 1;
CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
";
// The name Cursor gives a chat until it finds a title
const UNTITLED: &str = "New Agent";

/// Chats of [Cursor Agent](https://cursor.com/cli): `chats/<bucket>/<uuid>/store.db` under `~/.cursor`, a SQLite store of blobs holding protobuf state and the model's history as JSON messages.
///
/// The format is not published; the adapter follows the stores Cursor Agent 2026.09 writes. A written chat seeds the model's history, which keeps Cursor from adding its own system prompt and rules to that chat. Subagent chats stay out.
pub struct CursorAdapter {
    root: PathBuf,
}

impl CursorAdapter {
    /// An adapter for the Cursor configuration directory `root`.
    pub fn new(root: PathBuf) -> CursorAdapter {
        CursorAdapter { root }
    }

    /// The configuration directory: `CURSOR_CONFIG_DIR`, or else `cursor` under `XDG_CONFIG_HOME`, or else `~/.cursor`.
    pub fn default_root() -> Result<PathBuf> {
        if let Some(dir) = files::env_dir("CURSOR_CONFIG_DIR") {
            return Ok(dir);
        }
        if let Some(dir) = files::env_dir("XDG_CONFIG_HOME") {
            return Ok(dir.join("cursor"));
        }
        Ok(files::home()?.join(".cursor"))
    }
}

// Cursor files a chat under the MD5 of the directory it was started in
fn bucket(cwd: &str) -> String {
    use md5::Digest;

    base16ct::lower::encode_string(&md5::Md5::digest(cwd.as_bytes()))
}

// Every `chats/<bucket>/<session>/store.db` under the configuration directory
fn stores(root: &Path) -> Result<Vec<PathBuf>> {
    let mut stores = Vec::new();
    for bucket in files::read_dir(&root.join("chats"))? {
        if !bucket.is_dir() {
            continue;
        }
        for dir in files::read_dir(&bucket)? {
            let store = dir.join("store.db");
            if store.is_file() {
                stores.push(store);
            }
        }
    }
    Ok(stores)
}

// The session row of a store, which Cursor keeps as hex encoded JSON
fn meta(connection: &Connection) -> Result<Option<Value>> {
    let hex: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(hex) = hex else {
        return Ok(None);
    };
    let json = base16ct::mixed::decode_vec(&hex).context("reading Cursor's session row")?;
    Ok(Some(serde_json::from_slice(&json)?))
}

// `meta.json`, the listing Cursor keeps next to a store and rebuilds from it when missing
fn sidecar(store: &Path) -> Value {
    fs::read(store.with_file_name("meta.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn title(meta: &Value, sidecar: &Value) -> Option<String> {
    json::string(&sidecar["title"])
        .or_else(|| json::string(&meta["name"]).filter(|name| name != UNTITLED))
}

impl Source for CursorAdapter {
    fn agent(&self) -> Agent {
        Agent::Cursor
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let stores = match path {
            Some(store) if store.is_file() => vec![store.to_path_buf()],
            Some(root) => stores(root)?,
            None => stores(&self.root)?,
        };
        let mut sessions = Vec::new();
        for store in stores {
            let id = store
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .filter(|name| Uuid::parse_str(name).is_ok());
            let Some(id) = id else {
                continue;
            };
            let connection = sqlite::open_read_only(&store)?;
            let Some(meta) = meta(&connection)? else {
                continue;
            };
            let sidecar = sidecar(&store);
            // A subagent's chat reaches its parent as a tool result, and a chat without a root has no messages yet
            let subagent = meta["subagentInfo"].is_object() || sidecar["isSubagent"] == true;
            if subagent || json::string(&meta["latestRootBlobId"]).is_none() {
                continue;
            }
            sessions.push(NativeSession {
                id: id.to_owned(),
                stamp: sqlite::stamp(&store)?,
                title: title(&meta, &sidecar),
                path: store,
            });
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let mut connection = sqlite::open_read_only(&session.path)?;
        // Cursor writes blobs before the row that points at them, so one read transaction sees a whole chat
        let transaction = connection.transaction()?;
        let meta = meta(&transaction)?.context("the Cursor store has no session row")?;
        let load = |id: &str| -> Result<Vec<u8>> {
            transaction
                .query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .with_context(|| format!("loading Cursor blob {id}"))
        };
        let root = load(meta["latestRootBlobId"].as_str().unwrap_or_default())?;
        let graph = graph_parser::read(&root, meta["createdAt"].as_i64(), &load, blobs)?;
        let sidecar = sidecar(&session.path);
        Ok(NativeRead {
            info: Info {
                cwd: json::string(&sidecar["cwd"]).or(graph.cwd),
                title: title(&meta, &sidecar),
                created: graph.created,
            },
            drafts: graph.drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for CursorAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::uuid_v4();
        let cwd = export.cwd.to_string_lossy();
        let created = time::unix_millis(export.created);
        let graph = graph_writer::write(export.events, &cwd, created, blobs)?;
        // Cursor sends the key along with the blobs it uploads; the blobs on disk stay plain
        let key = format!(
            "{}{}",
            identifiers::short_hex(32),
            identifiers::short_hex(32)
        );
        let meta = json!({
            "agentId": id,
            "latestRootBlobId": graph.root,
            "name": export.title.unwrap_or(UNTITLED),
            "createdAt": created,
            "mode": "default",
            "isRunEverything": false,
            "blobEncryptionKey": key,
        });
        // The store is built aside and published whole, so Cursor never sees half of it
        let scratch = tempfile::NamedTempFile::new()?;
        {
            let mut connection = Connection::open(scratch.path())?;
            connection.execute_batch(SCHEMA)?;
            let transaction = connection.transaction()?;
            for (blob, data) in &graph.blobs {
                transaction.execute(
                    "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
                    params![blob, data],
                )?;
            }
            transaction.execute(
                "INSERT INTO meta (key, value) VALUES ('0', ?1)",
                [base16ct::lower::encode_string(meta.to_string().as_bytes())],
            )?;
            transaction.commit()?;
        }
        let dir = self.root.join("chats").join(bucket(&cwd)).join(&id);
        let store = dir.join("store.db");
        files::create_new(&store, &fs::read(scratch.path())?)?;
        let mut sidecar = json!({
            "schemaVersion": 1,
            "createdAtMs": created,
            "hasConversation": true,
            "updatedAtMs": time::unix_millis(&time::now()),
            "cwd": cwd,
        });
        if let Some(title) = export.title {
            sidecar["title"] = json!(title);
        }
        files::create_new(&dir.join("meta.json"), sidecar.to_string().as_bytes())?;
        Ok(NativeSession {
            id,
            stamp: sqlite::stamp(&store)?,
            path: store,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let dir = session
            .path
            .parent()
            .expect("session files have a directory");
        files::remove_file(&dir.join("meta.json"))?;
        files::remove_file(&session.path)?;
        files::remove_empty_dir(dir)
    }

    // Cursor Agent only looks among the chats of the directory it runs in
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("cursor-agent");
        command
            .arg(format!("--resume={}", session.id))
            .current_dir(cwd);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_hash_the_directory_like_cursor() {
        let cwd = format!("/tmp/sm-cursor-capture/{}/work", "x".repeat(47));
        assert_eq!(bucket(&cwd), "632ceb05b38b16cd23391476ee3aa80e");
    }
}
