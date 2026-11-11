use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use rusqlite::params;
use serde_json::Value;

use super::backup::Unpacked;
use super::{parser, writer};
use crate::support::{identifiers, sqlite};

/// Conversations of [RikkaHub](https://github.com/rikkahub/rikkahub) in the backups its Android app exports: rows of the database packed in a backup, read and inserted directly
///
/// The app keeps its conversations on the phone, so baton only sees a backup handed to it, and restoring that backup in the app brings the written sessions over
pub struct RikkahubAdapter {
    backup: Option<PathBuf>,
    unpacked: Mutex<HashMap<PathBuf, Arc<Unpacked>>>,
}

impl RikkahubAdapter {
    /// An adapter for `backup`, which discovering without a path lists and writing adds to, creating it when it does not exist; without one, discovering needs a path and writing fails
    pub fn new(backup: Option<PathBuf>) -> RikkahubAdapter {
        RikkahubAdapter {
            backup,
            unpacked: Mutex::new(HashMap::new()),
        }
    }

    // Each backup is unpacked once, so reads share its database and writes edit it until `flush`
    fn unpacked(&self, path: &Path, create: bool) -> Result<Arc<Unpacked>> {
        let mut unpacked = self.unpacked.lock().unwrap();
        if let Some(existing) = unpacked.get(path) {
            return Ok(Arc::clone(existing));
        }
        let (backup, exists) = resolve(path, create)?;
        let opened = if let Some(existing) = unpacked.get(&backup) {
            Arc::clone(existing)
        } else if exists {
            Arc::new(Unpacked::open(&backup)?)
        } else {
            Arc::new(Unpacked::create(&backup)?)
        };
        unpacked.insert(backup, Arc::clone(&opened));
        unpacked.insert(path.to_path_buf(), Arc::clone(&opened));

        Ok(opened)
    }
}

// Canonicalize the file if it exists, or its parent when baton is creating it
fn resolve(path: &Path, create: bool) -> Result<(PathBuf, bool)> {
    match fs::canonicalize(path) {
        Ok(file) => {
            ensure!(file.is_file(), "{} is not a backup file", path.display());
            Ok((file, true))
        }
        Err(error) if error.kind() == ErrorKind::NotFound && create => {
            let name = path
                .file_name()
                .context("the RikkaHub backup needs a file name")?;
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let folder = fs::canonicalize(parent).with_context(|| {
                format!(
                    "the RikkaHub backup directory {} must exist",
                    parent.display()
                )
            })?;
            ensure!(folder.is_dir(), "{} is not a directory", parent.display());
            Ok((folder.join(name), false))
        }
        Err(error) => {
            Err(error).with_context(|| format!("resolving RikkaHub backup {}", path.display()))
        }
    }
}

impl Source for RikkahubAdapter {
    fn agent(&self) -> Agent {
        Agent::Rikkahub
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let unpacked = match (path, self.backup.as_deref()) {
            (Some(path), _) => self.unpacked(path, false)?,
            (None, Some(path)) => self.unpacked(path, true)?,
            (None, None) => return Ok(Vec::new()),
        };
        let connection = sqlite::open_read_only(&unpacked.database())?;
        let mut statement =
            connection.prepare("SELECT id, title, update_at FROM ConversationEntity")?;
        let rows = statement.query_map([], |row| {
            let title: String = row.get(1)?;
            Ok(NativeSession {
                id: row.get(0)?,
                path: unpacked.path().to_path_buf(),
                stamp: Stamp {
                    size: 0,
                    mtime: row.get(2)?,
                },
                title: Some(title).filter(|title| !title.trim().is_empty()),
            })
        })?;

        Ok(rows.collect::<rusqlite::Result<Vec<NativeSession>>>()?)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let unpacked = self.unpacked(&session.path, false)?;
        // Parsing reads uploads under the lock writers hold while they wait for readers to leave the database, so the rows come first
        let (title, created, nodes) = {
            let mut connection = sqlite::open_read_only(&unpacked.database())?;
            let snapshot = connection.transaction()?;
            let (title, created): (String, i64) = snapshot.query_row(
                "SELECT title, create_at FROM ConversationEntity WHERE id = ?1",
                [&session.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut statement = snapshot.prepare(
                "SELECT messages, select_index FROM message_node WHERE conversation_id = ?1 ORDER BY node_index",
            )?;
            let nodes = statement
                .query_map([&session.id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            (title, created, nodes)
        };
        let created = time::from_unix_millis(created);
        let mut selected_messages = Vec::new();
        for (messages, selected) in nodes {
            let messages: Vec<Value> = serde_json::from_str(&messages).with_context(|| {
                format!("reading messages of RikkaHub conversation {}", session.id)
            })?;
            // The alternatives of a node are regenerations and edits, of which the app shows the selected one
            let Some(message) = usize::try_from(selected)
                .ok()
                .and_then(|index| messages.into_iter().nth(index))
            else {
                continue;
            };
            selected_messages.push(message);
        }
        let drafts = parser::drafts(&selected_messages, &unpacked, blobs)?;

        Ok(NativeRead {
            info: Info {
                cwd: None,
                title: Some(title).filter(|title| !title.trim().is_empty()),
                created,
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for RikkahubAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let backup = self.backup.as_deref().context(
            "RikkaHub runs on Android; pass --backup with a backup exported from the app",
        )?;
        let unpacked = self.unpacked(backup, true)?;
        let (messages, uploads) = writer::messages(export.events)?;
        let id = identifiers::session_v4(export.seed).to_string();
        let create_at = time::unix_millis(export.created);
        // The app lists the conversations of an assistant by when they were last updated
        let update_at = export
            .events
            .last()
            .map_or(create_at, |event| time::unix_millis(&event.timestamp));
        unpacked.add(&uploads, blobs, |transaction| {
            transaction.execute(
                "INSERT INTO ConversationEntity (id, assistant_id, title, nodes, create_at, update_at) VALUES (?1, ?2, ?3, '[]', ?4, ?5)",
                params![
                    id,
                    unpacked.assistant(),
                    export.title.unwrap_or_default(),
                    create_at,
                    update_at
                ],
            )?;
            let mut insert = transaction.prepare(
                "INSERT INTO message_node (id, conversation_id, node_index, messages, select_index) VALUES (?1, ?2, ?3, ?4, 0)",
            )?;
            for (index, message) in messages.iter().enumerate() {
                insert.execute(params![
                    identifiers::uuid_v4(),
                    id,
                    index as i64,
                    format!("[{message}]")
                ])?;
            }

            Ok(())
        })?;

        Ok(NativeSession {
            id,
            path: unpacked.path().to_path_buf(),
            stamp: Stamp {
                size: 0,
                mtime: update_at,
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        self.unpacked(&session.path, false)?.remove(&session.id)
    }

    fn flush(&self) -> Result<()> {
        for (path, unpacked) in &*self.unpacked.lock().unwrap() {
            if path == unpacked.path() {
                unpacked.pack()?;
            }
        }

        Ok(())
    }

    // The CLI refuses to resume RikkaHub before exporting anything
    fn resume_command(&self, _session: &NativeSession, _cwd: &Path) -> Command {
        unreachable!("baton cannot open RikkaHub, which runs on Android")
    }
}
