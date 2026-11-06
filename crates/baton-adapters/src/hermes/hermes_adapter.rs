use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use chrono::Local;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;

use crate::hermes::row_parser::{self, Row};
use crate::hermes::row_writer;
use crate::support::{files, identifiers, program, sqlite};

// The sessions Hermes lists: no delegated subagent runs, tool sessions or hidden plugin sessions, and child sessions only where they branched off or replaced their parent
const LISTED: &str = "
SELECT s.id FROM sessions s
WHERE (s.parent_session_id IS NULL
       OR json_extract(COALESCE(s.model_config, '{}'), '$._branched_from') IS NOT NULL
       OR EXISTS (SELECT 1 FROM sessions p WHERE p.id = s.parent_session_id
                  AND p.end_reason = 'branched' AND s.started_at >= p.ended_at)
       OR json_extract(COALESCE(s.model_config, '{}'), '$._reset_from') IS NOT NULL
       OR EXISTS (SELECT 1 FROM sessions p WHERE p.id = s.parent_session_id
                  AND p.end_reason IN ('session_reset', 'session_switch', 'idle', 'daily', 'suspended', 'resume_pending_expired')
                  AND s.session_key IS NOT NULL AND s.session_key != '' AND s.session_key = p.session_key))
  AND json_extract(COALESCE(s.model_config, '{}'), '$._delegate_from') IS NULL
  AND s.hidden = 0 AND s.source != 'tool'";

// The session a compression rotated into, picked the way Hermes picks it on resume
const CONTINUATION: &str = "
SELECT child.id FROM sessions parent JOIN sessions child ON child.parent_session_id = parent.id
WHERE parent.id = ?1 AND parent.end_reason = 'compression'
  AND json_extract(COALESCE(child.model_config, '{}'), '$._branched_from') IS NULL
  AND json_extract(COALESCE(child.model_config, '{}'), '$._delegate_from') IS NULL
  AND COALESCE(child.source, '') != 'tool'
ORDER BY CASE WHEN child.end_reason = 'compression' THEN 0 WHEN child.ended_at IS NULL THEN 1 ELSE 2 END,
         COALESCE((SELECT MAX(v) FROM (SELECT child.last_activity_at AS v UNION ALL
                   SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = child.id)), child.started_at) DESC,
         child.started_at DESC, child.id DESC
LIMIT 1";

// Active rows are what Hermes replays; rows baton archived behind its own summary are history Hermes no longer needs
const ROWS: &str = "
SELECT id, role, COALESCE(content, ''), tool_call_id, tool_calls, effect_disposition, timestamp,
       COALESCE(NULLIF(reasoning, ''), NULLIF(reasoning_content, '')),
       COALESCE(reasoning_details, codex_reasoning_items), _compressed_summary
FROM messages
WHERE session_id = ?1
  AND (active = 1 OR (compacted = 1 AND id <= COALESCE(
        (SELECT json_extract(model_config, '$._baton_compacted_through') FROM sessions WHERE id = ?1), -1)))
ORDER BY id";

/// Sessions of [Hermes Agent](https://github.com/NousResearch/hermes-agent): rows of the SQLite database `state.db` in its home directory, read and inserted directly.
///
/// A session is the active rows of its messages; when Hermes rotates a session on compaction, the hidden session it continues in is read as part of it.
pub struct HermesAdapter {
    home: PathBuf,
}

impl HermesAdapter {
    /// An adapter for the Hermes home directory `home`, which holds `state.db`.
    pub fn new(home: PathBuf) -> HermesAdapter {
        HermesAdapter { home }
    }

    /// The Hermes home directory: `HERMES_HOME`, or else `~/.hermes` (`%LOCALAPPDATA%\hermes` on Windows), or the `profiles/<name>` directory below it while a profile other than the default one is active.
    pub fn default_home() -> Result<PathBuf> {
        let native = if cfg!(windows) {
            files::local_app_data()?.join("hermes")
        } else {
            files::home()?.join(".hermes")
        };
        let home = files::env_dir("HERMES_HOME");
        if let Some(home) = &home
            && home
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "profiles")
        {
            return Ok(home.clone());
        }
        let base = home.unwrap_or_else(|| native.clone());
        // The active profile is recorded in the native home, unless HERMES_HOME lies elsewhere
        let root = if base.starts_with(&native) {
            &native
        } else {
            &base
        };
        let profile = fs::read_to_string(root.join("active_profile")).unwrap_or_default();
        let profile = profile.trim().to_lowercase();
        if profile.is_empty() || profile == "default" {
            return Ok(base);
        }
        Ok(base.join("profiles").join(profile))
    }
}

impl Source for HermesAdapter {
    fn agent(&self) -> Agent {
        Agent::Hermes
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(home) => home.join("state.db"),
            None => self.home.join("state.db"),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        let ids = {
            let mut statement = connection.prepare(LISTED)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            let lineage = lineage(&connection, &id)?;
            sessions.push(NativeSession {
                path: db.clone(),
                stamp: stamp(&connection, &lineage)?,
                title: title(&connection, &lineage)?,
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
        let mut connection = sqlite::open_read_only(&session.path)?;
        let snapshot = connection.transaction()?;
        let lineage = lineage(&snapshot, &session.id)?;
        let (cwd, started): (Option<String>, f64) = snapshot.query_row(
            "SELECT cwd, started_at FROM sessions WHERE id = ?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let info = Info {
            cwd: cwd.filter(|cwd| !cwd.is_empty()),
            title: title(&snapshot, &lineage)?,
            created: time::from_unix_millis((started * 1000.0).round() as i64),
        };
        let mut drafts = Vec::new();
        let mut statement = snapshot.prepare(ROWS)?;
        for id in &lineage {
            let rows = statement.query_map([id], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    tool_call_id: row.get(3)?,
                    tool_calls: row.get(4)?,
                    effect_disposition: row.get(5)?,
                    timestamp: row.get(6)?,
                    reasoning: row.get(7)?,
                    reasoning_details: row.get(8)?,
                    summary: row.get(9)?,
                })
            })?;
            for row in rows {
                drafts.extend(row_parser::drafts(&row?, blobs)?);
            }
        }
        Ok(NativeRead {
            info,
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for HermesAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let db = self.home.join("state.db");
        if !db.is_file() {
            bail!(
                "Hermes has not created {} yet, run `hermes` once first",
                db.display()
            );
        }
        let created = export.created;
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        let rows = row_writer::rows(export.events, blobs)?;
        // Rows before the last summary are archived the way Hermes archives compacted rows, so Hermes replays from the summary on
        let first_active = rows.iter().rposition(|row| row.summary).unwrap_or(0);
        let active = (rows.len() - first_active) as i64;
        let tool_calls: usize = rows[first_active..]
            .iter()
            .map(|row| row.tool_calls.len())
            .sum();
        let ended = rows.last().map_or(0.0, |row| row.timestamp);
        let started = started.with_timezone(&Local).format("%Y%m%d_%H%M%S");
        let mut connection = sqlite::open_read_write(&db)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = unused_id(&transaction, &started.to_string())?;
        let title = match export.title {
            Some(title) => free_title(&transaction, title)?,
            None => None,
        };
        transaction.execute(
            "INSERT INTO sessions (id, source, started_at, ended_at, end_reason, message_count, tool_call_count, cwd, title)
             VALUES (?1, 'cli', ?2, ?3, 'cli_close', ?4, ?5, ?6, ?7)",
            params![
                id,
                time::unix_millis(created) as f64 / 1000.0,
                ended,
                active,
                tool_calls as i64,
                export.cwd.to_string_lossy(),
                title,
            ],
        )?;
        let mut newest = 0;
        for (index, row) in rows.iter().enumerate() {
            let archived = index < first_active;
            let calls = (!row.tool_calls.is_empty()).then(|| json!(row.tool_calls).to_string());
            let finish = (row.role == "assistant").then_some(if calls.is_some() {
                "tool_calls"
            } else {
                "stop"
            });
            transaction.execute(
                "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, finish_reason,
                                       timestamp, observed, _compressed_summary, active, compacted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11)",
                params![
                    id,
                    row.role,
                    row.content,
                    row.tool_call_id,
                    calls,
                    row.tool_name,
                    finish,
                    row.timestamp,
                    row.summary,
                    !archived,
                    archived,
                ],
            )?;
            newest = transaction.last_insert_rowid();
            if index + 1 == first_active {
                let marker = json!({"_baton_compacted_through": newest}).to_string();
                transaction.execute(
                    "UPDATE sessions SET model_config = ?2 WHERE id = ?1",
                    params![id, marker],
                )?;
            }
        }
        transaction.commit()?;
        Ok(NativeSession {
            id,
            path: db,
            stamp: Stamp {
                size: active as u64,
                mtime: newest,
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let mut connection = sqlite::open_read_write(&session.path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_children: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE parent_session_id = ?1)",
            [&session.id],
            |row| row.get(0),
        )?;
        if has_children {
            bail!(
                "cannot remove Hermes session {} with child sessions",
                session.id
            );
        }
        transaction.execute("DELETE FROM messages WHERE session_id = ?1", [&session.id])?;
        transaction.execute("DELETE FROM sessions WHERE id = ?1", [&session.id])?;
        transaction.commit()?;
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("hermes");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}

// A session followed by the sessions its compressions rotated it into
fn lineage(connection: &Connection, root: &str) -> Result<Vec<String>> {
    let mut lineage = vec![root.to_owned()];
    let mut statement = connection.prepare(CONTINUATION)?;
    loop {
        let tip = lineage.last().expect("a lineage starts with its root");
        let next: Option<String> = statement.query_row([tip], |row| row.get(0)).optional()?;
        match next {
            Some(next) if !lineage.contains(&next) => lineage.push(next),
            _ => return Ok(lineage),
        }
    }
}

// Appends and compactions raise the newest row id, rewinds lower the number of active rows
fn stamp(connection: &Connection, lineage: &[String]) -> Result<Stamp> {
    let mut stamp = Stamp { size: 0, mtime: 0 };
    for id in lineage {
        let (active, newest): (i64, i64) = connection.query_row(
            "SELECT COUNT(*) FILTER (WHERE active = 1), COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        stamp.size += active as u64;
        stamp.mtime = stamp.mtime.max(newest);
    }
    Ok(stamp)
}

// Rotation carries the title over as `title #2`, so the newest session of a lineage with a title names it
fn title(connection: &Connection, lineage: &[String]) -> Result<Option<String>> {
    let mut statement = connection.prepare("SELECT title FROM sessions WHERE id = ?1")?;
    for id in lineage.iter().rev() {
        let title: Option<String> = statement.query_row([id], |row| row.get(0))?;
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            return Ok(Some(title));
        }
    }
    Ok(None)
}

// Hermes names sessions after their local start time and a random suffix
fn unused_id(transaction: &Transaction, started: &str) -> Result<String> {
    loop {
        let id = format!("{started}_{}", identifiers::short_hex(6));
        let taken: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !taken {
            return Ok(id);
        }
    }
}

// Titles are unique across Hermes; a numbered `title #2` would flow back and rename the baton session, so a taken title is left out
fn free_title(transaction: &Transaction, title: &str) -> Result<Option<String>> {
    let taken: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE title = ?1)",
        [title],
        |row| row.get(0),
    )?;
    Ok((!taken).then(|| title.to_owned()))
}
