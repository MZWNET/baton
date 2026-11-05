use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Connection, params, params_from_iter};

use crate::cache::database;
use crate::{Agent, Stamp, Store};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    cwd TEXT,
    created TEXT NOT NULL,
    updated TEXT NOT NULL,
    events INTEGER NOT NULL,
    meta_size INTEGER NOT NULL,
    meta_mtime INTEGER NOT NULL,
    log_size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS links (
    agent TEXT NOT NULL,
    native_id TEXT NOT NULL,
    session TEXT NOT NULL,
    PRIMARY KEY (agent, native_id)
);
CREATE VIRTUAL TABLE IF NOT EXISTS texts USING fts5(
    session UNINDEXED,
    kind UNINDEXED,
    ts UNINDEXED,
    body,
    tokenize = 'trigram'
);
";

/// A rebuildable SQLite index of the data directory in `index.db`, for listing, searching and resolving sessions.
///
/// [`Baton::open`](crate::Baton::open) brings the index up to date, including sessions a sync tool brought in from elsewhere.
pub struct Index {
    db: Connection,
}

/// One session as listed by [`Index::list`].
pub struct SessionSummary {
    /// The session id.
    pub id: String,
    /// The session title.
    pub title: Option<String>,
    /// The project directory the session ran in.
    pub cwd: Option<String>,
    /// The timestamp of the newest event.
    pub updated: String,
    /// How many events the session holds, including thinking and meta events.
    pub events: i64,
    /// The agents of the native sessions linked to the session.
    pub agents: Vec<String>,
}

/// An event whose text matched [`Index::search`].
pub struct SearchHit {
    /// The session holding the event.
    pub session: String,
    /// The session title.
    pub title: Option<String>,
    /// The event kind, such as `user` or `tool_result`.
    pub kind: String,
    /// When the event happened.
    pub timestamp: String,
    /// The searchable text of the event, as given by [`Body::text`](crate::Body::text).
    pub body: String,
}

struct Indexed {
    meta: Stamp,
    log_size: u64,
    events: i64,
}

impl Index {
    pub(crate) fn open(cache: &Path, data: &Path) -> Result<Index> {
        let db = database::open(
            &cache.join("index.db"),
            data,
            SCHEMA,
            &["sessions", "links", "texts"],
        )?;
        Ok(Index { db })
    }

    // Indexes what changed in the data directory since the last refresh, reading only appended events
    pub(crate) fn refresh(&mut self, store: &Store) -> Result<()> {
        let transaction = self.db.transaction()?;
        let mut indexed: HashMap<String, Indexed> = HashMap::new();
        {
            let mut statement = transaction
                .prepare("SELECT id, meta_size, meta_mtime, log_size, events FROM sessions")?;
            let rows = statement.query_map([], |row| {
                let indexed = Indexed {
                    meta: Stamp {
                        size: row.get::<_, i64>(1)? as u64,
                        mtime: row.get(2)?,
                    },
                    log_size: row.get::<_, i64>(3)? as u64,
                    events: row.get(4)?,
                };
                Ok((row.get::<_, String>(0)?, indexed))
            })?;
            for row in rows {
                let (id, row) = row?;
                indexed.insert(id, row);
            }
        }
        let ids: HashSet<String> = store.ids()?.into_iter().collect();
        for gone in indexed.keys() {
            if !ids.contains(gone) {
                transaction.execute("DELETE FROM sessions WHERE id = ?1", [gone])?;
                transaction.execute("DELETE FROM links WHERE session = ?1", [gone])?;
                transaction.execute("DELETE FROM texts WHERE session = ?1", [gone])?;
            }
        }
        for id in &ids {
            let meta_stamp = Stamp::of(&store.meta_path(id))?;
            let log_size = fs::metadata(store.events_path(id)).map_or(0, |metadata| metadata.len());
            let previous = indexed.get(id);
            if previous.is_some_and(|row| row.meta == meta_stamp && row.log_size == log_size) {
                continue;
            }
            let meta = store.meta(id)?;
            let (from, mut events) = match previous {
                Some(row) if row.log_size <= log_size => (row.log_size, row.events),
                _ => {
                    transaction.execute("DELETE FROM texts WHERE session = ?1", [id])?;
                    (0, 0)
                }
            };
            let log = store.events_from(id, from)?;
            for event in &log.events {
                if event.body.replayable() {
                    transaction.execute(
                        "INSERT INTO texts (session, kind, ts, body) VALUES (?1, ?2, ?3, ?4)",
                        params![id, event.body.kind(), event.timestamp, event.body.text()],
                    )?;
                }
            }
            events += log.events.len() as i64;
            transaction.execute(
                "INSERT OR REPLACE INTO sessions (id, title, cwd, created, updated, events, meta_size, meta_mtime, log_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    meta.title,
                    meta.cwd,
                    meta.created,
                    meta.updated,
                    events,
                    meta_stamp.size as i64,
                    meta_stamp.mtime,
                    log.end as i64,
                ],
            )?;
            transaction.execute("DELETE FROM links WHERE session = ?1", [id])?;
            for link in &meta.links {
                transaction.execute(
                    "INSERT OR REPLACE INTO links (agent, native_id, session) VALUES (?1, ?2, ?3)",
                    params![link.agent.name(), link.native_id, id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// The session linked to the native session `native_id` of `agent`.
    pub fn linked_session(&self, agent: Agent, native_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;

        let session = self
            .db
            .query_row(
                "SELECT session FROM links WHERE agent = ?1 AND native_id = ?2",
                params![agent.name(), native_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(session)
    }

    /// The session linked to the native session of `agent` whose id starts with `prefix`; fails when several sessions match.
    pub fn linked_session_by_prefix(&self, agent: Agent, prefix: &str) -> Result<Option<String>> {
        let mut statement = self.db.prepare(
            "SELECT DISTINCT session FROM links WHERE agent = ?1 AND substr(native_id, 1, length(?2)) = ?2",
        )?;
        let rows = statement.query_map(params![agent.name(), prefix], |row| row.get(0))?;
        let sessions = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        unique(sessions, &format!("{agent}:{prefix}"))
    }

    /// The session whose id is `prefix` or uniquely starts with it; fails when several sessions match.
    pub fn resolve(&self, prefix: &str) -> Result<Option<String>> {
        let mut statement = self
            .db
            .prepare("SELECT id FROM sessions WHERE substr(id, 1, length(?1)) = ?1")?;
        let rows = statement.query_map([prefix], |row| row.get(0))?;
        let ids = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        if ids.iter().any(|id| id == prefix) {
            return Ok(Some(prefix.to_owned()));
        }
        unique(ids, prefix)
    }

    pub(crate) fn title(&self, session: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;

        let title = self
            .db
            .query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                [session],
                |row| row.get(0),
            )
            .optional()?;
        Ok(title.flatten())
    }

    /// Sessions from the project directory `cwd` and linked to `agent`, each filter applying only when given, most recently updated first.
    pub fn list(&self, cwd: Option<&str>, agent: Option<Agent>) -> Result<Vec<SessionSummary>> {
        let mut statement = self.db.prepare(
            "SELECT id, title, cwd, updated, events,
                    (SELECT group_concat(DISTINCT agent) FROM links WHERE links.session = sessions.id)
             FROM sessions
             WHERE (?1 IS NULL OR cwd = ?1)
               AND (?2 IS NULL OR id IN (SELECT session FROM links WHERE agent = ?2))
             ORDER BY updated DESC",
        )?;
        let rows = statement.query_map(params![cwd, agent.map(Agent::name)], |row| {
            let agents: Option<String> = row.get(5)?;
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                cwd: row.get(2)?,
                updated: row.get(3)?,
                events: row.get(4)?,
                agents: agents.map_or_else(Vec::new, |agents| {
                    agents.split(',').map(String::from).collect()
                }),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<SessionSummary>>>()?)
    }

    /// The newest replayable events containing every one of `terms`, compared case-insensitively, at most `limit` of them.
    ///
    /// Terms of three or more characters are looked up in the trigram index and shorter ones are scanned for, so substrings match in any language.
    pub fn search(&self, terms: &[String], limit: usize) -> Result<Vec<SearchHit>> {
        // Trigram MATCH cannot find shorter terms, and SQLite never hands a LIKE with an ESCAPE clause to the index
        let (indexed, scanned): (Vec<&String>, Vec<&String>) =
            terms.iter().partition(|term| term.chars().count() >= 3);
        let mut conditions = Vec::new();
        let mut values = Vec::new();
        if !indexed.is_empty() {
            let mut phrases = Vec::with_capacity(indexed.len());
            for term in indexed {
                phrases.push(format!("\"{}\"", term.replace('"', "\"\"")));
            }
            conditions.push("t.texts MATCH ?");
            values.push(phrases.join(" AND "));
        }
        for term in scanned {
            let escaped = term
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            conditions.push("t.body LIKE ? ESCAPE '\\'");
            values.push(format!("%{escaped}%"));
        }
        let sql = format!(
            "SELECT t.session, s.title, t.kind, t.ts, t.body FROM texts t JOIN sessions s ON s.id = t.session
             WHERE {} ORDER BY t.ts DESC LIMIT {limit}",
            conditions.join(" AND ")
        );
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok(SearchHit {
                session: row.get(0)?,
                title: row.get(1)?,
                kind: row.get(2)?,
                timestamp: row.get(3)?,
                body: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<SearchHit>>>()?)
    }
}

fn unique(mut matches: Vec<String>, query: &str) -> Result<Option<String>> {
    match matches.len() {
        0 | 1 => Ok(matches.pop()),
        count => bail!("`{query}` matches {count} sessions, give more characters"),
    }
}
