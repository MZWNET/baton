use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use rusqlite::{TransactionBehavior, params};

use crate::devin::node_parser::{self, Node};
use crate::devin::node_writer;
use crate::support::{files, identifiers, program, sqlite};

// The main chain from its root to the tip `?2`; retries, edits and subagents leave other trees in the forest
const CHAIN: &str = "
WITH RECURSIVE chain(node_id, parent_node_id, chat_message, created_at, metadata, depth) AS (
  SELECT node_id, parent_node_id, chat_message, created_at, metadata, 0
  FROM message_nodes WHERE session_id = ?1 AND node_id = ?2
  UNION ALL
  SELECT m.node_id, m.parent_node_id, m.chat_message, m.created_at, m.metadata, c.depth + 1
  FROM chain c JOIN message_nodes m ON m.session_id = ?1 AND m.node_id = c.parent_node_id
  WHERE c.depth < 100000
)
SELECT node_id, chat_message, created_at, json_extract(metadata, '$.summarized_from') IS NOT NULL
FROM chain ORDER BY depth DESC";

/// Sessions of [Devin CLI](https://docs.devin.ai/cli): rows of its SQLite database `sessions.db`, read and inserted directly.
///
/// The messages of a session form a forest whose active conversation is the chain ending at the session's `main_chain_id`.
pub struct DevinAdapter {
    db: PathBuf,
}

impl DevinAdapter {
    /// An adapter for the Devin session database `db`.
    pub fn new(db: PathBuf) -> DevinAdapter {
        DevinAdapter { db }
    }

    /// The session database: `sessions.db` in `DEVIN_SESSIONS_DIR`, or else in `devin/cli` under `XDG_DATA_HOME` or `~/.local/share`, or under `%APPDATA%` on Windows.
    pub fn default_db() -> Result<PathBuf> {
        if let Some(dir) = files::env_dir("DEVIN_SESSIONS_DIR") {
            return Ok(dir.join("sessions.db"));
        }
        let data = if cfg!(windows) {
            files::roaming_app_data()?
        } else {
            files::xdg_data_home()?
        };
        Ok(data.join("devin").join("cli").join("sessions.db"))
    }
}

impl Source for DevinAdapter {
    fn agent(&self) -> Agent {
        Agent::Devin
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(dir) => dir.join("sessions.db"),
            None => self.db.clone(),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        // Hidden sessions belong to helper agents such as the summarizer; node rows only ever grow, while the activity time also covers renames
        let mut statement = connection.prepare(
            "SELECT s.id, s.title, s.last_activity_at,
                    (SELECT MAX(m.row_id) FROM message_nodes m WHERE m.session_id = s.id)
             FROM sessions s WHERE s.hidden = 0 AND s.main_chain_id IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            let title: Option<String> = row.get(1)?;
            let newest: Option<i64> = row.get(3)?;
            Ok(NativeSession {
                id: row.get(0)?,
                path: db.clone(),
                stamp: Stamp {
                    size: newest.unwrap_or_default() as u64,
                    mtime: row.get(2)?,
                },
                title: title.filter(|title| !title.trim().is_empty()),
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
        let (cwd, title, created, tip): (String, Option<String>, i64, i64) = snapshot.query_row(
            "SELECT working_directory, title, created_at, main_chain_id FROM sessions WHERE id = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let mut statement = snapshot.prepare(CHAIN)?;
        let nodes = statement.query_map(params![session.id, tip], |row| {
            Ok(Node {
                id: row.get(0)?,
                chat_message: row.get(1)?,
                created: row.get(2)?,
                summarized: row.get(3)?,
            })
        })?;
        let mut seen = HashSet::new();
        let mut drafts = Vec::new();
        for node in nodes {
            let node = node?;
            // A damaged forest can loop back onto its own chain
            if !seen.insert(node.id) {
                break;
            }
            drafts.extend(node_parser::drafts(&node, blobs)?);
        }
        Ok(NativeRead {
            info: Info {
                cwd: Some(cwd),
                title: title.filter(|title| !title.trim().is_empty()),
                created: time::from_unix_millis(created * 1000),
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for DevinAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        if !self.db.is_file() {
            bail!(
                "Devin has not created {} yet, run `devin` once first",
                self.db.display()
            );
        }
        let nodes = node_writer::nodes(export.events, blobs)?;
        let id = identifiers::uuid_v4();
        // The newest session is the one Devin lists first and continues
        let activity = time::unix_millis(&time::now()).div_euclid(1000);
        let mut connection = sqlite::open_read_write(&self.db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The fallback model and the default mode the CLI writes itself, so a resumed session keeps the user's own permissions
        transaction.execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id, hidden)
             VALUES (?1, ?2, 'windsurf', 'swe-1-6-fast', 'normal', ?3, ?4, ?5, ?6, 0)",
            params![
                id,
                export.cwd.to_string_lossy(),
                time::unix_millis(export.created).div_euclid(1000),
                activity,
                export.title,
                nodes.len() as i64 - 1,
            ],
        )?;
        let mut newest = 0;
        for (index, node) in nodes.iter().enumerate() {
            transaction.execute(
                "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    index as i64,
                    index.checked_sub(1).map(|parent| parent as i64),
                    node.chat_message.to_string(),
                    node.created,
                ],
            )?;
            newest = transaction.last_insert_rowid();
        }
        transaction.commit()?;
        Ok(NativeSession {
            id,
            path: self.db.clone(),
            stamp: Stamp {
                size: newest as u64,
                mtime: activity,
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let mut connection = sqlite::open_read_write(&session.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM message_nodes WHERE session_id = ?1",
            [&session.id],
        )?;
        transaction.execute("DELETE FROM sessions WHERE id = ?1", [&session.id])?;
        transaction.commit()?;
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("devin");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
