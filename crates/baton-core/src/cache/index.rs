use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::cache::database;
use crate::project::{identify, matches};
use crate::storage::record_order;
use crate::{Agent, Direction, Project, Store};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    cwd TEXT,
    project_remote TEXT,
    project_root TEXT,
    project_path TEXT,
    created TEXT NOT NULL,
    updated TEXT NOT NULL,
    events INTEGER NOT NULL,
    alternate_branches INTEGER NOT NULL,
    leaf TEXT,
    replayable INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS links (
    session TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    host TEXT NOT NULL,
    agent TEXT NOT NULL,
    native_id TEXT NOT NULL,
    removed INTEGER NOT NULL,
    removable INTEGER NOT NULL,
    PRIMARY KEY (session, name)
);
CREATE INDEX IF NOT EXISTS links_lookup ON links (host, agent, native_id, version, name);
CREATE TABLE IF NOT EXISTS files (
    session TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (session, kind, name)
);
CREATE TABLE IF NOT EXISTS events (
    session TEXT NOT NULL,
    id TEXT NOT NULL,
    version TEXT NOT NULL,
    name TEXT NOT NULL,
    parent TEXT,
    ts TEXT NOT NULL,
    replayable INTEGER NOT NULL,
    reset INTEGER NOT NULL,
    PRIMARY KEY (session, id)
);
CREATE INDEX IF NOT EXISTS events_parent ON events (session, parent);
CREATE VIRTUAL TABLE IF NOT EXISTS texts USING fts5(
    session UNINDEXED,
    event UNINDEXED,
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
    host: String,
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
    /// Whether the session has alternate branches.
    pub alternate_branches: bool,
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

/// What materializing needs to know about a session, read without loading it.
pub(crate) struct MaterializationSession {
    pub id: String,
    /// The newest leaf by timestamp and id, which ends the active branch.
    pub leaf: Option<String>,
    /// Whether the active branch holds replayable events after its last reset.
    pub replayable: bool,
}

/// A native session whose newest link on this host belongs to `session`.
pub(crate) struct MaterializationLink {
    pub session: String,
    pub native_id: String,
    /// Whether the link is an untouched export with a recorded digest, so the native session may be removed once obsolete.
    pub removable: bool,
}

impl Index {
    pub(crate) fn open(cache: &Path, data: &Path, host: &str) -> Result<Index> {
        let path = cache.join("index.db");
        let db = Connection::open(&path)?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 4 {
            db.execute_batch("BEGIN; DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS texts; DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS events; PRAGMA user_version = 4; COMMIT;")?;
        }
        drop(db);
        let db = database::open(
            &path,
            data,
            SCHEMA,
            &["sessions", "links", "texts", "files", "events"],
        )?;
        Ok(Index {
            db,
            host: host.to_owned(),
        })
    }

    // Indexes new immutable files and rebuilds sessions when indexed files disappear
    pub(crate) fn refresh(&mut self, store: &Store) -> Result<()> {
        let transaction = self.db.transaction()?;
        let mut indexed: HashMap<String, HashSet<(String, String)>> = HashMap::new();
        {
            let mut statement = transaction.prepare("SELECT id FROM sessions")?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                indexed.insert(row?, HashSet::new());
            }
            let mut statement = transaction.prepare("SELECT session, kind, name FROM files")?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (id, kind, name) = row?;
                indexed.entry(id).or_default().insert((kind, name));
            }
        }
        let ids: HashSet<String> = store.ids()?.into_iter().collect();
        for gone in indexed.keys() {
            if !ids.contains(gone) {
                remove_session(&transaction, gone)?;
            }
        }
        for id in &ids {
            let event_files = store.files(id, "events")?;
            let record_files = store.files(id, "records")?;
            let current: HashSet<(String, String)> = event_files
                .iter()
                .map(|name| ("events".to_owned(), name.clone()))
                .chain(
                    record_files
                        .iter()
                        .map(|name| ("records".to_owned(), name.clone())),
                )
                .collect();
            let empty = HashSet::new();
            let mut previous = indexed.get(id).unwrap_or(&empty);
            let rebuild = !previous.is_subset(&current);
            if rebuild {
                remove_session(&transaction, id)?;
                previous = &empty;
            }
            let new_session = rebuild || !indexed.contains_key(id);
            if !new_session && previous == &current {
                continue;
            }
            if new_session {
                transaction.execute("INSERT INTO sessions (id, created, updated, events, alternate_branches, replayable) VALUES (?1, '', '', 0, 0, 0)", [id])?;
            }
            let added = current.difference(previous).collect::<Vec<_>>();
            let mut new_events = added
                .iter()
                .filter(|(kind, _)| kind == "events")
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
            let events_changed = new_session || !new_events.is_empty();
            new_events.sort_by(|a, b| record_order(a).cmp(&record_order(b)));
            for name in new_events {
                let (version, _) = record_order(name);
                for event in store.segment(id, name)? {
                    let previous: Option<(String, String)> = transaction
                        .query_row(
                            "SELECT version, name FROM events WHERE session = ?1 AND id = ?2",
                            params![id, event.id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    if let Some((previous_version, previous_name)) = previous {
                        if (version, name.as_str())
                            >= (previous_version.as_str(), previous_name.as_str())
                        {
                            continue;
                        }
                        transaction.execute(
                            "DELETE FROM texts WHERE session = ?1 AND event = ?2",
                            params![id, event.id],
                        )?;
                    }
                    transaction.execute(
                        "INSERT OR REPLACE INTO events (session, id, version, name, parent, ts, replayable, reset) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![id, event.id, version, name, event.parent, event.timestamp, event.body.replayable(), event.body.is_reset()],
                    )?;
                    if event.body.replayable() {
                        transaction.execute(
                            "INSERT INTO texts (session, event, kind, ts, body) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![id, event.id, event.body.kind(), event.timestamp, event.body.text()],
                        )?;
                    }
                }
            }
            if new_session || added.iter().any(|(kind, _)| kind == "records") {
                let meta = store.record_meta(id)?;
                transaction.execute(
                    "UPDATE sessions SET title = ?2, cwd = ?3, project_remote = ?4, project_root = ?5, project_path = ?6 WHERE id = ?1",
                    params![id, meta.title, meta.cwd, meta.project.as_ref().and_then(|project| project.remote.as_deref()), meta.project.as_ref().and_then(|project| project.root.as_deref()), meta.project.as_ref().map(|project| project.path.as_str())],
                )?;
                transaction.execute("DELETE FROM links WHERE session = ?1", [id])?;
                for (name, link) in store.link_versions(id)? {
                    let (version, _) = record_order(&name);
                    let removable = link.direction == Direction::Export
                        && !link.continued
                        && link.exported_digest.is_some();
                    transaction.execute(
                        "INSERT INTO links (session, name, version, host, agent, native_id, removed, removable) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![id, name, version, link.host, link.agent.name(), link.native_id, link.removed, removable],
                    )?;
                }
            }
            if events_changed {
                transaction.execute(
                    "UPDATE sessions SET
                    created = COALESCE((SELECT MIN(ts) FROM events WHERE session = ?1), ''),
                    updated = COALESCE((SELECT MAX(ts) FROM events WHERE session = ?1), ''),
                    events = (SELECT COUNT(*) FROM events WHERE session = ?1),
                    alternate_branches = (SELECT COUNT(*) > 1 FROM events e WHERE e.session = ?1 AND NOT EXISTS (SELECT 1 FROM events child WHERE child.session = e.session AND child.parent = e.id)),
                    leaf = (SELECT e.id FROM events e WHERE e.session = ?1 AND NOT EXISTS (SELECT 1 FROM events child WHERE child.session = e.session AND child.parent = e.id) ORDER BY e.ts DESC, e.id DESC LIMIT 1)
                 WHERE id = ?1",
                    [id],
                )?;
                transaction.execute(
                    "WITH RECURSIVE branch(id, parent, reset, replayable) AS (
                     SELECT e.id, e.parent, e.reset, e.replayable FROM events e JOIN sessions s ON s.id = e.session AND s.leaf = e.id WHERE e.session = ?1
                     UNION
                     SELECT e.id, e.parent, e.reset, e.replayable FROM branch b JOIN events e ON e.session = ?1 AND e.id = b.parent WHERE NOT b.reset
                 )
                 UPDATE sessions SET replayable = EXISTS (SELECT 1 FROM branch WHERE replayable) WHERE id = ?1",
                    [id],
                )?;
            }
            for (kind, name) in added {
                transaction.execute(
                    "INSERT INTO files (session, kind, name) VALUES (?1, ?2, ?3)",
                    params![id, kind, name],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// The session linked to the native session `native_id` of `agent`.
    pub fn linked_session(&self, agent: Agent, native_id: &str) -> Result<Option<String>> {
        let session = self
            .db
            .query_row(
                "SELECT session FROM (SELECT session, removed FROM links WHERE host = ?1 AND agent = ?2 AND native_id = ?3 ORDER BY version DESC, name DESC, session DESC LIMIT 1) WHERE NOT removed",
                params![self.host, agent.name(), native_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(session)
    }

    pub(crate) fn materialization_sessions(&self) -> Result<Vec<MaterializationSession>> {
        let mut statement = self
            .db
            .prepare("SELECT id, leaf, replayable FROM sessions ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(MaterializationSession {
                id: row.get(0)?,
                leaf: row.get(1)?,
                replayable: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn materialization_links(&self, agent: Agent) -> Result<Vec<MaterializationLink>> {
        let mut statement = self.db.prepare(
            "SELECT l.session, l.native_id, l.removable FROM links l WHERE l.host = ?1 AND l.agent = ?2 AND NOT l.removed
             AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session))
             ORDER BY l.session, l.native_id",
        )?;
        let rows = statement.query_map(params![self.host, agent.name()], |row| {
            Ok(MaterializationLink {
                session: row.get(0)?,
                native_id: row.get(1)?,
                removable: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn sessions_without_project(&self) -> Result<Vec<(String, Option<String>)>> {
        let mut statement = self
            .db
            .prepare("SELECT id, cwd FROM sessions WHERE project_path IS NULL ORDER BY id")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The session linked to the native session of `agent` whose id starts with `prefix`; fails when several sessions match.
    pub fn linked_session_by_prefix(&self, agent: Agent, prefix: &str) -> Result<Option<String>> {
        let mut statement = self.db.prepare(
            "SELECT DISTINCT l.session FROM links l WHERE l.agent = ?1 AND substr(l.native_id, 1, length(?2)) = ?2 AND NOT l.removed
             AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session))",
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
            "WITH active_links AS (
                 SELECT l.session, l.agent FROM links l
                 WHERE NOT l.removed AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session))
             )
             SELECT id, title, cwd, updated, events, alternate_branches,
                    (SELECT group_concat(DISTINCT agent) FROM active_links WHERE active_links.session = sessions.id),
                    project_remote, project_root, project_path
             FROM sessions
             WHERE (?1 IS NULL OR id IN (SELECT session FROM active_links WHERE agent = ?1))
             ORDER BY updated DESC",
        )?;
        let rows = statement.query_map(params![agent.map(Agent::name)], |row| {
            let agents: Option<String> = row.get(6)?;
            let path: Option<String> = row.get(9)?;
            let remote = row.get(7)?;
            let root = row.get(8)?;
            let project = path.map(|path| Project { remote, root, path });
            Ok((
                SessionSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    cwd: row.get(2)?,
                    updated: row.get(3)?,
                    events: row.get(4)?,
                    alternate_branches: row.get(5)?,
                    agents: agents.map_or_else(Vec::new, |agents| {
                        agents.split(',').map(String::from).collect()
                    }),
                },
                project,
            ))
        })?;
        let identity = cwd.and_then(|cwd| identify(Path::new(cwd)));
        let mut sessions = Vec::new();
        for row in rows {
            let (summary, project) = row?;
            if cwd.is_none_or(|cwd| {
                matches(
                    Path::new(cwd),
                    identity.as_ref(),
                    summary.cwd.as_deref(),
                    project.as_ref(),
                )
            }) {
                sessions.push(summary);
            }
        }
        Ok(sessions)
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

fn remove_session(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<()> {
    transaction.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    for table in ["links", "texts", "files", "events"] {
        transaction.execute(&format!("DELETE FROM {table} WHERE session = ?1"), [id])?;
    }
    Ok(())
}
