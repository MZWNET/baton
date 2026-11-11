use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use baton_core::{
    Adapter, Agent, Blobs, Body, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    time,
};
use chrono::DateTime;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::antigravity::parser::{self, USER_INPUT};
use crate::antigravity::writer::{self, Step};
use crate::support::protobuf::Encoder;
use crate::support::{files, identifiers, program, sqlite, uri};

// The tables agy creates in every conversation database
const CONVERSATION_SCHEMA: &str = r#"
CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));
CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));
CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT "main",`data` blob,PRIMARY KEY (`id`));
CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));
CREATE INDEX `idx_steps_status` ON `steps`(`status`);
CREATE INDEX `idx_steps_step_type` ON `steps`(`step_type`);
PRAGMA user_version = 1;
"#;

// The picker index agy keeps next to the conversations
const SUMMARY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT "",`preview` text NOT NULL DEFAULT "",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`status` text NOT NULL DEFAULT "",`source` text NOT NULL DEFAULT "",`project_id` text NOT NULL DEFAULT "",`agent_name` text NOT NULL DEFAULT "",`parent_conversation_id` text NOT NULL DEFAULT "",`nesting_depth` integer NOT NULL DEFAULT 0,`battle_id` text NOT NULL DEFAULT "",`winning_conversation_id` text NOT NULL DEFAULT "",`not_fully_idle` numeric NOT NULL DEFAULT false,`killed` numeric NOT NULL DEFAULT false,`last_user_input_time` datetime NOT NULL,`last_user_input_step_index` integer NOT NULL DEFAULT -1,`app_data_dir` text NOT NULL DEFAULT "",PRIMARY KEY (`conversation_id`));
CREATE INDEX IF NOT EXISTS `idx_conversation_summaries_last_user_input_time` ON `conversation_summaries`(`last_user_input_time`);
CREATE INDEX IF NOT EXISTS `idx_conversation_summaries_last_modified_time` ON `conversation_summaries`(`last_modified_time`);
"#;

// The project agy files a conversation under when none was chosen
const PROJECT: &str = "default-cli-project";
// Trajectory type and source of a CLI conversation
const CASCADE: i64 = 4;
const CLI: i64 = 17;

/// Conversations of [Antigravity CLI](https://antigravity.google/docs/cli/): `conversations/<uuid>.db` under `~/.gemini/antigravity-cli`, a SQLite database of protobuf steps per conversation, listed in `conversation_summaries.db`.
///
/// The step format is not published; the adapter follows what agy 1.1.16 writes. Subagent conversations stay out.
pub struct AntigravityAdapter {
    root: PathBuf,
}

impl AntigravityAdapter {
    /// An adapter for the Antigravity CLI data directory `root`.
    pub fn new(root: PathBuf) -> AntigravityAdapter {
        AntigravityAdapter { root }
    }

    /// The data directory, `~/.gemini/antigravity-cli`.
    pub fn default_root() -> Result<PathBuf> {
        Ok(files::home()?.join(".gemini").join("antigravity-cli"))
    }

    // The summary rows of top-level conversations by id; children of other conversations have none
    fn summaries(root: &Path) -> Result<HashMap<String, Row>> {
        let db = root.join("conversation_summaries.db");
        let mut rows = HashMap::new();
        if !db.is_file() {
            return Ok(rows);
        }
        let connection = sqlite::open_read_only(&db)?;
        let mut statement = connection.prepare(
            "SELECT conversation_id, title, workspace_uris, parent_conversation_id = '' AND nesting_depth = 0
             FROM conversation_summaries",
        )?;
        let found = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, Row::from_row(row, 1)?))
        })?;
        for row in found {
            let (id, row) = row?;
            rows.insert(id, row);
        }
        Ok(rows)
    }

    // Without its summary row the conversation does not show up in agy's picker
    fn insert_summary(&self, id: &str, link: &str, export: &Export, steps: &[Step]) -> Result<()> {
        let db = self.root.join("conversation_summaries.db");
        let new = !db.exists();
        let mut connection = sqlite::open_or_create(&db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SUMMARY_SCHEMA)?;
        if new {
            transaction.execute_batch("PRAGMA user_version = 1")?;
        }
        let modified = steps
            .last()
            .map_or_else(|| time::unix_millis(export.created), |step| step.millis);
        let preview = export.events.iter().find_map(|event| match &event.body {
            Body::User(user) => Some(user.text.chars().take(16_384).collect::<String>()),
            _ => None,
        });
        let (last_input, last_input_time) = match steps
            .iter()
            .enumerate()
            .rev()
            .find(|(_, step)| step.kind == USER_INPUT)
        {
            Some((index, step)) => (index as i64, go_time(step.millis)),
            None => (-1, "0001-01-01 00:00:00+00:00".to_owned()),
        };
        transaction.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris,
                                                project_id, last_user_input_time, last_user_input_step_index, app_data_dir)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'antigravity-cli')",
            params![
                id,
                export.title.unwrap_or_default(),
                preview.unwrap_or_default(),
                steps.len() as i64,
                go_time(modified),
                json!([link]).to_string(),
                PROJECT,
                last_input_time,
                last_input,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

// What the summary row of a conversation says about it
struct Row {
    title: Option<String>,
    cwd: Option<String>,
    top_level: bool,
}

impl Row {
    fn from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Row> {
        let uris: String = row.get(offset + 1)?;
        Ok(Row {
            title: Some(row.get::<_, String>(offset)?).filter(|title| !title.is_empty()),
            cwd: serde_json::from_str::<Value>(&uris)
                .ok()
                .and_then(|uris| uris[0].as_str().and_then(uri::path)),
            top_level: row.get(offset + 2)?,
        })
    }
}

impl Source for AntigravityAdapter {
    fn agent(&self) -> Agent {
        Agent::Antigravity
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let (root, dbs) = parser::databases(&self.root, path)?;
        let summaries = AntigravityAdapter::summaries(root)?;
        let summary_db = root.join("conversation_summaries.db");
        let summary_stamp = summary_db
            .is_file()
            .then(|| sqlite::stamp(&summary_db))
            .transpose()?;
        let mut sessions = Vec::new();
        for (db, id) in dbs {
            let row = summaries.get(&id);
            if row.is_some_and(|row| !row.top_level) {
                continue;
            }
            let mut stamp = sqlite::stamp(&db)?;
            if let Some(summary) = summary_stamp {
                stamp.size = stamp.size.wrapping_add(summary.size);
                stamp.mtime = stamp.mtime.wrapping_add(summary.mtime);
            }
            sessions.push(NativeSession {
                stamp,
                title: row.and_then(|row| row.title.clone()),
                path: db,
                id,
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
        let connection = sqlite::open_read_only(&session.path)?;
        let conversation = parser::read(&connection, blobs)?;
        let root = session.path.parent().and_then(Path::parent);
        let row = match root.map(|root| root.join("conversation_summaries.db")) {
            Some(db) if db.is_file() => {
                let connection = sqlite::open_read_only(&db)?;
                connection
                    .query_row(
                        "SELECT title, workspace_uris, parent_conversation_id = '' AND nesting_depth = 0 FROM conversation_summaries WHERE conversation_id = ?1",
                        [&session.id],
                        |row| Row::from_row(row, 0),
                    )
                    .optional()?
            }
            _ => None,
        };
        let row = row.as_ref();
        Ok(NativeRead {
            info: Info {
                cwd: row.and_then(|row| row.cwd.clone()).or(conversation.cwd),
                title: row.and_then(|row| row.title.clone()),
                created: conversation.created,
            },
            drafts: conversation.drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for AntigravityAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::session_v4(export.seed).to_string();
        let link = uri::from_path(&export.cwd.to_string_lossy());
        let steps = writer::steps(export.events, blobs)?;
        let created = time::unix_millis(export.created);
        let db = self.root.join("conversations").join(format!("{id}.db"));
        sqlite::publish_new(&db, |connection| {
            connection.execute_batch(CONVERSATION_SCHEMA)?;
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO trajectory_meta (trajectory_id, cascade_id, trajectory_type, source) VALUES (?1, ?2, ?3, ?4)",
                params![identifiers::uuid_v4(), id, CASCADE, CLI],
            )?;
            let metadata = Encoder::default()
                .message(1, Encoder::default().string(1, &link))
                .message(2, writer::timestamp(created))
                .string(6, &id)
                .string(7, &link)
                .string(18, PROJECT)
                .finish();
            transaction.execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                [metadata],
            )?;
            {
                let mut insert = transaction.prepare(
                    "INSERT INTO steps (idx, step_type, status, has_subtrajectory, metadata, step_payload, step_format)
                     VALUES (?1, ?2, ?3, 0, ?4, ?5, 0)",
                )?;
                for (index, step) in steps.iter().enumerate() {
                    insert.execute(params![
                        index as i64,
                        step.kind as i64,
                        step.status as i64,
                        step.metadata,
                        step.payload
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })?;
        if let Err(error) = self.insert_summary(&id, &link, export, &steps) {
            fs::remove_file(&db)?;
            return Err(error);
        }
        Ok(NativeSession {
            id,
            stamp: sqlite::stamp(&db)?,
            path: db,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let root = session
            .path
            .parent()
            .and_then(Path::parent)
            .expect("conversation databases have a root");
        let summaries = root.join("conversation_summaries.db");
        let exists = match fs::metadata(&summaries) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if exists {
            let mut connection = sqlite::open_read_write(&summaries)?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "DELETE FROM conversation_summaries WHERE conversation_id = ?1",
                [&session.id],
            )?;
            files::remove_file(&session.path)?;
            transaction.commit()?;
        } else {
            files::remove_file(&session.path)?;
        }
        Ok(())
    }

    // agy lists the conversations of the directory it runs in
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("agy");
        command
            .arg("--conversation")
            .arg(&session.id)
            .current_dir(cwd);
        command
    }
}

// Go's text for a time, which the summary database stores
fn go_time(millis: i64) -> String {
    let time = DateTime::from_timestamp_millis(millis).unwrap_or_default();
    let mut text = time.format("%Y-%m-%d %H:%M:%S").to_string();
    let fraction = millis.rem_euclid(1000);
    if fraction != 0 {
        text.push_str(format!(".{fraction:03}").trim_end_matches('0'));
    }
    text.push_str("+00:00");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_times_read_like_go_ones() {
        assert_eq!(go_time(1_793_880_000_000), "2026-11-05 12:00:00+00:00");
        assert_eq!(go_time(1_793_880_001_250), "2026-11-05 12:00:01.25+00:00");
    }
}
