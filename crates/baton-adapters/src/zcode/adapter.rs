use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use rusqlite::{TransactionBehavior, params};
use serde_json::Value;

use crate::support::{files, program, sqlite};
use crate::zcode::parser::{self, Message};
use crate::zcode::writer;

/// Sessions of [ZCode](https://github.com/zai-org/ZCode): rows of its CLI database `db.sqlite`, read and inserted directly.
///
/// Rewinds select the active branch, and images live inline or in ZCode's artifact files.
pub struct ZcodeAdapter {
    db: PathBuf,
    artifacts: PathBuf,
}

impl ZcodeAdapter {
    /// An adapter for the ZCode session database and attachment artifacts.
    pub fn new(db: PathBuf, artifacts: PathBuf) -> ZcodeAdapter {
        ZcodeAdapter { db, artifacts }
    }

    /// The session database: `ZCODE_SESSION_DB_PATH`, `ZCODE_SESSION_DB`, or `~/.zcode/cli/db/db.sqlite`.
    pub fn default_db() -> Result<PathBuf> {
        if let Some(db) =
            files::env_dir("ZCODE_SESSION_DB_PATH").or_else(|| files::env_dir("ZCODE_SESSION_DB"))
        {
            return Ok(db);
        }
        Ok(files::home()?
            .join(".zcode")
            .join("cli")
            .join("db")
            .join("db.sqlite"))
    }

    /// The directory where ZCode keeps attachment artifacts, under `ZCODE_STORAGE_DIR` or `~/.zcode`.
    pub fn default_artifacts() -> Result<PathBuf> {
        let root = match files::env_dir("ZCODE_STORAGE_DIR") {
            Some(root) => root,
            None => files::home()?.join(".zcode"),
        };
        Ok(root.join("cli").join("artifacts"))
    }
}

impl Source for ZcodeAdapter {
    fn agent(&self) -> Agent {
        Agent::Zcode
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = sqlite::resolve_path(path, &self.db, "db.sqlite");
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        // ZCode folds subagent and workflow child sessions into their parents in history
        let mut statement = connection.prepare(
            "SELECT id, title, time_updated FROM session
             WHERE time_deleted IS NULL
               AND task_type IN ('interactive', 'fork', 'workflow_parent')
               AND NOT EXISTS (SELECT 1 FROM session_purge WHERE session_id = session.id)",
        )?;
        let rows = statement.query_map([], |row| {
            let title: String = row.get(1)?;
            Ok(NativeSession {
                id: row.get(0)?,
                path: db.clone(),
                stamp: Stamp {
                    size: 0,
                    mtime: row.get(2)?,
                },
                title: (!title.is_empty()).then_some(title),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let mut connection = sqlite::open_read_only(&session.path)?;
        let snapshot = connection.transaction()?;
        let (cwd, title, created, revert): (String, String, i64, Option<String>) = snapshot
            .query_row(
                "SELECT directory, title, time_created, revert FROM session WHERE id = ?1",
                [&session.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
        {
            let mut statement = snapshot.prepare(
                "SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY message_id, sequence IS NULL, sequence, time_created, id",
            )?;
            let rows = statement.query_map([&session.id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, message_id, data) = row?;
                let mut value: Value = serde_json::from_str(&data)
                    .with_context(|| format!("invalid JSON in ZCode part {id}"))?;
                value
                    .as_object_mut()
                    .with_context(|| format!("ZCode part {id} is not an object"))?
                    .insert("id".to_owned(), Value::String(id));
                parts.entry(message_id).or_default().push(value);
            }
        }
        let mut messages = Vec::new();
        {
            let mut statement = snapshot.prepare(
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY sequence IS NULL, sequence, time_created, rowid",
            )?;
            let rows = statement.query_map([&session.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, data) = row?;
                let data = serde_json::from_str(&data)
                    .with_context(|| format!("invalid JSON in ZCode message {id}"))?;
                messages.push(Message {
                    parts: parts.remove(&id).unwrap_or_default(),
                    id,
                    data,
                });
            }
        }
        let created = time::from_unix_millis(created);
        let fallback = created.clone().unwrap_or_else(time::now);
        let drafts = parser::read(messages, revert, &fallback, blobs, &self.artifacts)?;
        Ok(NativeRead {
            info: Info {
                cwd: Some(cwd),
                title: (!title.is_empty()).then_some(title),
                created,
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for ZcodeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        if !self.db.is_file() {
            bail!(
                "ZCode has not created {} yet, run `zcode` once first",
                self.db.display()
            );
        }
        let session = writer::session(export, blobs)?;
        let mut connection = sqlite::open_read_write(&self.db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // ZCode uses 0.0.0 when its version is unavailable
        transaction.execute(
            "INSERT INTO session (id, project_id, slug, directory, path, title, title_source, version, permission, task_type, trace_id, time_created, time_updated, time_title_updated)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, '0.0.0', '{\"mode\":\"build\"}', 'interactive', ?7, ?8, ?9, ?9)",
            params![session.id, session.project_id, session.slug, session.directory, session.title, session.title_source, session.trace_id, session.created, session.updated],
        )?;
        {
            let mut insert_message = transaction.prepare(
                "INSERT INTO message (id, session_id, time_created, time_updated, data, sequence) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut insert_part = transaction.prepare(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data, sequence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (sequence, message) in session.messages.iter().enumerate() {
                insert_message.execute(params![
                    message.id,
                    session.id,
                    message.created,
                    message.updated,
                    message.data.to_string(),
                    sequence as i64
                ])?;
                for (sequence, part) in message.parts.iter().enumerate() {
                    insert_part.execute(params![
                        part.id,
                        message.id,
                        session.id,
                        part.created,
                        part.created,
                        part.data.to_string(),
                        sequence as i64
                    ])?;
                }
            }
        }
        if let Some(selection) = &session.model_selection {
            transaction.execute(
                "INSERT INTO session_entry (id, session_id, type, time_created, time_updated, data) VALUES (?1, ?2, 'runtime/model_selection', ?3, ?3, ?4)",
                params![format!("{}:runtime-model-selection", session.id), session.id, session.updated, selection.to_string()],
            )?;
        }
        transaction.commit()?;
        Ok(NativeSession {
            id: session.id,
            path: self.db.clone(),
            stamp: Stamp {
                size: 0,
                mtime: session.updated,
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let mut connection = sqlite::open_read_write(&session.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM part WHERE session_id = ?1", [&session.id])?;
        transaction.execute("DELETE FROM message WHERE session_id = ?1", [&session.id])?;
        transaction.execute(
            "DELETE FROM session_entry WHERE session_id = ?1",
            [&session.id],
        )?;
        transaction.execute("DELETE FROM session WHERE id = ?1", [&session.id])?;
        transaction.commit()?;
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("zcode");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
