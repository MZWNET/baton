use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use rusqlite::{TransactionBehavior, params};
use serde_json::{Value, json};

use crate::mastracode::message_parser::{self, Row};
use crate::mastracode::{message_writer, project};
use crate::support::{files, identifiers, program, sqlite};

// The tables MastraCode creates on every start, so baton can create a missing database the same way
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mastra_threads (
  "id" TEXT NOT NULL PRIMARY KEY,
  "resourceId" TEXT NOT NULL,
  "title" TEXT NOT NULL,
  "metadata" TEXT,
  "createdAt" TEXT NOT NULL,
  "updatedAt" TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS mastra_messages (
  "id" TEXT NOT NULL PRIMARY KEY,
  "thread_id" TEXT NOT NULL,
  "content" TEXT NOT NULL,
  "role" TEXT NOT NULL,
  "type" TEXT NOT NULL,
  "createdAt" TEXT NOT NULL,
  "resourceId" TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_thread_created_at ON mastra_messages (thread_id, "createdAt");
CREATE INDEX IF NOT EXISTS idx_messages_thread_resource_created_at ON mastra_messages (thread_id, "resourceId", "createdAt");
"#;

/// Sessions of [MastraCode](https://github.com/mastra-ai/mastra/tree/main/mastracode): threads of its SQLite database `mastra.db`, read and inserted directly.
///
/// MastraCode folds tool results into the calls that produced them and files threads under a resource derived from the project's git remote or directory, which a written thread gets too.
pub struct MastracodeAdapter {
    db: PathBuf,
}

impl MastracodeAdapter {
    /// An adapter for the MastraCode database `db`.
    pub fn new(db: PathBuf) -> MastracodeAdapter {
        MastracodeAdapter { db }
    }

    /// The database: `MASTRA_DB_PATH`, or else `mastra.db` in `MASTRA_APP_DATA_DIR` or in the platform's application data directory.
    pub fn default_db() -> Result<PathBuf> {
        if let Some(db) = files::env_dir("MASTRA_DB_PATH") {
            return Ok(db);
        }
        let dir = match files::env_dir("MASTRA_APP_DATA_DIR") {
            Some(dir) => dir,
            None if cfg!(target_os = "macos") => files::home()?
                .join("Library")
                .join("Application Support")
                .join("mastracode"),
            None if cfg!(windows) => files::roaming_app_data()?.join("mastracode"),
            None => files::xdg_data_home()?.join("mastracode"),
        };
        Ok(dir.join("mastra.db"))
    }
}

impl Source for MastracodeAdapter {
    fn agent(&self) -> Agent {
        Agent::Mastracode
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(dir) => dir.join("mastra.db"),
            None => self.db.clone(),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        // Forked subagent threads are clones MastraCode hides itself
        let mut statement = connection.prepare(
            "SELECT t.id, t.title, t.\"updatedAt\" FROM mastra_threads t
             WHERE EXISTS (SELECT 1 FROM mastra_messages m WHERE m.thread_id = t.id)
               AND NOT (json_valid(t.metadata, 5) AND COALESCE(json_extract(t.metadata, '$.forkedSubagent'), 0) = 1)",
        )?;
        let rows = statement.query_map([], |row| {
            let title: String = row.get(1)?;
            let updated: String = row.get(2)?;
            Ok(NativeSession {
                id: row.get(0)?,
                path: db.clone(),
                stamp: Stamp {
                    size: 0,
                    mtime: time::unix_millis(&updated),
                },
                title: (!title.is_empty()).then_some(title),
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
        let mut connection = sqlite::open_read_only(&session.path)?;
        let snapshot = connection.transaction()?;
        let (title, created, cwd): (String, String, Option<String>) = snapshot.query_row(
            "SELECT title, \"createdAt\", CASE WHEN json_valid(metadata, 5) THEN json_extract(metadata, '$.projectPath') END
             FROM mastra_threads WHERE id = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut statement = snapshot.prepare(
            "SELECT id, role, \"createdAt\", content FROM mastra_messages WHERE thread_id = ?1 ORDER BY \"createdAt\", rowid",
        )?;
        let rows = statement.query_map([&session.id], |row| {
            Ok(Row {
                id: row.get(0)?,
                role: row.get(1)?,
                created: row.get(2)?,
                content: row.get(3)?,
            })
        })?;
        let mut drafts = Vec::new();
        for row in rows {
            drafts.extend(message_parser::drafts(&row?, blobs)?);
        }
        Ok(NativeRead {
            info: Info {
                cwd,
                title: (!title.is_empty()).then_some(title),
                created: time::normalize(&created),
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for MastracodeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let project = project::detect(export.cwd);
        let messages = message_writer::messages(export.events, blobs)?;
        // The newest thread of a project is the one MastraCode resumes
        let mut updated = time::now();
        if let Some(last) = messages.last() {
            updated = updated.max(last.created.clone());
        }
        let id = identifiers::uuid_v4();
        let mut connection = sqlite::open_or_create(&self.db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA)?;
        transaction.execute(
            "INSERT INTO mastra_threads (id, \"resourceId\", title, metadata, \"createdAt\", \"updatedAt\")
             VALUES (?1, ?2, ?3, jsonb(?4), ?5, ?6)",
            params![
                id,
                project.resource,
                export.title.unwrap_or_default(),
                json!({"projectPath": project.path}).to_string(),
                time::normalize(export.created).unwrap_or_else(|| export.created.to_owned()),
                updated,
            ],
        )?;
        for message in &messages {
            transaction.execute(
                "INSERT INTO mastra_messages (id, thread_id, content, role, type, \"createdAt\", \"resourceId\")
                 VALUES (?1, ?2, ?3, ?4, 'v2', ?5, ?6)",
                params![
                    identifiers::uuid_v4(),
                    id,
                    Value::to_string(&message.content),
                    message.role,
                    message.created,
                    project.resource,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(NativeSession {
            id,
            path: self.db.clone(),
            stamp: Stamp {
                size: 0,
                mtime: time::unix_millis(&updated),
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let mut connection = sqlite::open_read_write(&session.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM mastra_messages WHERE thread_id = ?1",
            [&session.id],
        )?;
        transaction.execute("DELETE FROM mastra_threads WHERE id = ?1", [&session.id])?;
        transaction.commit()?;
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("mastracode");
        command.arg("resume").arg(&session.id).current_dir(cwd);
        command
    }
}
