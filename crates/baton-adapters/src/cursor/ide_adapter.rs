use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use baton_core::{
    Agent, Blobs, Cursor, Info, NativeRead, NativeSession, Progress, Source, Stamp, time,
};
use serde_json::Value;

use crate::cursor::{bubble_parser, graph_parser};
use crate::support::{files, images, json, sqlite, uri};

/// Chats of the [Cursor](https://cursor.com/) editor: composers in its global state database, `User/globalStorage/state.vscdb` in the editor's user data directory.
///
/// baton reads them but cannot write them: the running editor rewrites that database from memory, and no command opens a given chat. Agent chats hold the same graph as Cursor Agent's; older chats are read from their message bubbles. Subagent chats stay out.
pub struct CursorIdeAdapter {
    user_data: PathBuf,
}

impl CursorIdeAdapter {
    /// An adapter for the editor's user data directory `user_data`.
    pub fn new(user_data: PathBuf) -> CursorIdeAdapter {
        CursorIdeAdapter { user_data }
    }

    /// The editor's user data directory, `Cursor` in the platform's application data or configuration directory.
    pub fn default_user_data() -> Result<PathBuf> {
        files::editor_data("Cursor")
    }
}

fn database(user_data: &Path) -> PathBuf {
    user_data
        .join("User")
        .join("globalStorage")
        .join("state.vscdb")
}

// The folder of a workspace the editor opened, from `workspaceStorage/<id>/workspace.json` beside the global storage
fn folder(db: &Path, workspace: &str) -> Option<String> {
    let user = db.parent()?.parent()?;
    let file = fs::read(
        user.join("workspaceStorage")
            .join(workspace)
            .join("workspace.json"),
    )
    .ok()?;
    let file: Value = serde_json::from_slice(&file).ok()?;
    uri::path(file["folder"].as_str()?)
}

impl Source for CursorIdeAdapter {
    fn agent(&self) -> Agent {
        Agent::CursorIde
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(db) if db.is_file() => db.to_path_buf(),
            Some(user_data) => database(user_data),
            None => database(&self.user_data),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        let mut statement = connection.prepare(
            "SELECT composerId, lastUpdatedAt, CAST(value AS TEXT) FROM composerHeaders WHERE NOT COALESCE(isSubagent, 0)",
        )?;
        let rows = statement.query_map([], |row| {
            let header: Option<String> = row.get(2)?;
            let header: Value = header
                .and_then(|header| serde_json::from_str(&header).ok())
                .unwrap_or_default();
            Ok(NativeSession {
                id: row.get(0)?,
                path: db.clone(),
                stamp: Stamp {
                    size: 0,
                    mtime: row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                },
                title: json::string(&header["name"]),
            })
        })?;
        let mut sessions = Vec::new();
        for session in rows {
            sessions.push(session?);
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
        let transaction = connection.transaction()?;
        let (workspace, created, header): (Option<String>, Option<i64>, Option<String>) = transaction.query_row(
            "SELECT workspaceId, createdAt, CAST(value AS TEXT) FROM composerHeaders WHERE composerId = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let header: Value = header
            .and_then(|header| serde_json::from_str(&header).ok())
            .unwrap_or_default();
        let composer: String = transaction
            .query_row(
                "SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1",
                [format!("composerData:{}", session.id)],
                |row| row.get(0),
            )
            .with_context(|| format!("loading Cursor composer {}", session.id))?;
        let composer: Value = serde_json::from_str(&composer)?;
        let created = created.or(composer["createdAt"].as_i64());
        // Agent chats carry their graph's root as `~` and base64
        let state = composer["conversationState"]
            .as_str()
            .and_then(|state| state.strip_prefix('~'));
        let (drafts, logged, created) = match state {
            Some(state) => {
                let root = images::decode(state).context("reading Cursor's conversation state")?;
                let load = |id: &str| -> Result<Vec<u8>> {
                    transaction
                        .prepare_cached("SELECT value FROM cursorDiskKV WHERE key = ?1")?
                        .query_row([format!("agentKv:blob:{id}")], |row| row.get(0))
                        .with_context(|| format!("loading Cursor blob {id}"))
                };
                let graph = graph_parser::read(&root, created, &load, blobs)?;
                (graph.drafts, graph.cwd, graph.created)
            }
            None => {
                let created = created.and_then(time::from_unix_millis);
                let start = created.clone().unwrap_or_else(time::now);
                (
                    bubble_parser::read(&transaction, &session.id, &composer, &start)?,
                    None,
                    created,
                )
            }
        };
        // The workspace storage maps the composer's workspace to its folder, which wins over what the header claims
        let cwd = workspace
            .and_then(|workspace| folder(&session.path, &workspace))
            .or_else(|| json::string(&header["workspaceIdentifier"]["uri"]["fsPath"]))
            .or(logged);
        Ok(NativeRead {
            info: Info {
                cwd,
                title: json::string(&header["name"]).or_else(|| json::string(&composer["name"])),
                created,
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}
