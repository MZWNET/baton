use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::{fs, thread};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, params};

use crate::cache::database;
use crate::project::{identify, matches};
use crate::storage::record_order;
use crate::{Agent, Direction, Link, Project, SessionMeta, Store};

const SCHEMA_VERSION: i64 = 10;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    cwd TEXT,
    project_remote TEXT,
    project_root TEXT,
    project_path TEXT,
    updated TEXT NOT NULL,
    events INTEGER NOT NULL,
    alternate_branches INTEGER NOT NULL,
    leaf TEXT,
    replayable INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS links (
    session TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    host TEXT NOT NULL,
    agent TEXT NOT NULL,
    native_id TEXT NOT NULL,
    path TEXT NOT NULL,
    removed INTEGER NOT NULL,
    removable INTEGER NOT NULL,
    writer_version INTEGER NOT NULL,
    PRIMARY KEY (session, name)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS links_lookup ON links (host, agent, native_id, version, name);
CREATE INDEX IF NOT EXISTS links_native_id ON links (native_id, agent, host, version, name);
CREATE TABLE IF NOT EXISTS files (
    session TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (session, kind, name)
) WITHOUT ROWID;
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
";

/// A rebuildable SQLite index of the data directory in `index.db`, for listing and resolving sessions
///
/// [`Baton::open`](crate::Baton::open) brings the index up to date, including sessions a sync tool brought in from elsewhere.
pub struct Index {
    db: Mutex<Connection>,
    host: String,
}

struct Pending {
    id: String,
    rebuild: bool,
    new_session: bool,
    segments: Vec<String>,
    records: Option<Vec<String>>,
    added: Vec<(String, String)>,
}

struct IndexedEvent {
    id: String,
    parent: Option<String>,
    timestamp: String,
    replayable: bool,
    reset: bool,
}

struct Loaded {
    pending: Pending,
    segments: Vec<(String, Vec<IndexedEvent>)>,
    records: Option<(SessionMeta, Vec<(String, Link)>)>,
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
    /// The writer version of an export
    pub writer_version: u32,
}

impl Index {
    pub(crate) fn open(cache: &Path, data: &Path, host: &str) -> Result<Index> {
        let path = cache.join("index.db");
        let version: i64 = {
            let db = Connection::open(&path)?;

            db.pragma_query_value(None, "user_version", |row| row.get(0))?
        };
        if version != SCHEMA_VERSION {
            // Dropping old tables would walk every page
            Self::remove(cache)?;
        }
        let db = database::open(
            &path,
            data,
            SCHEMA,
            &["sessions", "links", "files", "events"],
        )?;
        if version != SCHEMA_VERSION {
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        db.set_prepared_statement_cache_capacity(48);

        Ok(Index {
            db: Mutex::new(db),
            host: host.to_owned(),
        })
    }

    pub(crate) fn remove(cache: &Path) -> Result<()> {
        for name in ["index.db", "index.db-wal", "index.db-shm"] {
            let path = cache.join(name);
            match fs::remove_file(&path) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    return Err(error).with_context(|| format!("removing {}", path.display()));
                }
                _ => {}
            }
        }

        Ok(())
    }

    // Indexes new immutable files and rebuilds sessions when indexed files disappear
    pub(crate) fn refresh(&mut self, store: &Store) -> Result<()> {
        let transaction = self.db.get_mut().unwrap().transaction()?;
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
        let sessions = store.session_files()?;
        let ids: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
        for gone in indexed.keys() {
            if !ids.contains(gone.as_str()) {
                remove_session(&transaction, gone)?;
            }
        }
        let mut pending = Vec::new();
        for session in sessions {
            let id = &session.id;
            let current: HashSet<(String, String)> = session
                .events
                .iter()
                .map(|name| ("events".to_owned(), name.clone()))
                .chain(
                    session
                        .records
                        .iter()
                        .map(|name| ("records".to_owned(), name.clone())),
                )
                .collect();
            let empty = HashSet::new();
            let mut previous = indexed.get(id).unwrap_or(&empty);
            let rebuild = !previous.is_subset(&current);
            if rebuild {
                previous = &empty;
            }
            let new_session = rebuild || !indexed.contains_key(id);
            if !new_session && previous == &current {
                continue;
            }
            let added = current.difference(previous).cloned().collect::<Vec<_>>();
            let mut segments = added
                .iter()
                .filter(|(kind, _)| kind == "events")
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>();
            segments.sort_by(|a, b| record_order(a).cmp(&record_order(b)));
            let records = (new_session || added.iter().any(|(kind, _)| kind == "records"))
                .then_some(session.records);
            pending.push(Pending {
                id: session.id,
                rebuild,
                new_session,
                segments,
                records,
                added,
            });
        }
        let (sender, receiver) = mpsc::sync_channel(rayon::current_num_threads());
        thread::scope(|scope| -> Result<()> {
            scope.spawn(move || {
                pending
                    .into_par_iter()
                    .try_for_each_with(sender, |sender, pending| {
                        sender.send(load(store, pending)).ok()
                    })
            });
            for loaded in receiver {
                apply(&transaction, loaded?)?;
            }
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }

    /// The session linked to the native session `native_id` of `agent`.
    pub fn linked_session(&self, agent: Agent, native_id: &str) -> Result<Option<String>> {
        self.find_linked_session(Some(agent), native_id, Some(&self.host))
    }

    /// The newest active link to a native session on any host, optionally filtered by agent.
    pub fn linked_session_any_host(
        &self,
        agent: Option<Agent>,
        native_id: &str,
    ) -> Result<Option<String>> {
        self.find_linked_session(agent, native_id, None)
    }

    fn find_linked_session(
        &self,
        agent: Option<Agent>,
        native_id: &str,
        host: Option<&str>,
    ) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let session = db.prepare_cached(
            "SELECT l.session FROM links l WHERE (?1 IS NULL OR l.agent = ?1) AND l.native_id = ?2 AND (?3 IS NULL OR l.host = ?3) AND NOT l.removed AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session)) ORDER BY l.version DESC, l.name DESC, l.session DESC LIMIT 1",
        )?.query_row(
            params![agent.map(Agent::name), native_id, host],
            |row| row.get(0),
        ).optional()?;
        Ok(session)
    }

    /// Finds an active provider link by file path on any host.
    pub fn linked_file(&self, agent: Agent, path: &Path) -> Result<Option<(String, String)>> {
        let db = self.db.lock().unwrap();
        let link = db.prepare_cached(
            "SELECT l.session, l.native_id FROM links l WHERE l.agent = ?1 AND l.path = ?2 AND NOT l.removed AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session)) ORDER BY l.version DESC, l.name DESC, l.session DESC LIMIT 1",
        )?.query_row(
            params![agent.name(), path.to_string_lossy()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        Ok(link)
    }

    /// The newest active native link for a session and agent on this host.
    pub fn latest_native(&self, session: &str, agent: Agent) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let native_id = db.prepare_cached(
            "SELECT l.native_id FROM links l WHERE l.session = ?1 AND l.host = ?2 AND l.agent = ?3 AND NOT l.removed AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session)) ORDER BY l.version DESC, l.name DESC LIMIT 1",
        )?.query_row(
            params![session, self.host, agent.name()], |row| row.get(0),
        ).optional()?;
        Ok(native_id)
    }

    pub(crate) fn registry_links(&self, agent: Agent) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare_cached(
            "SELECT l.session, l.name FROM links l WHERE l.agent = ?1 AND NOT l.removed AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session)) ORDER BY l.version DESC, l.name DESC, l.session DESC"
        )?;
        let rows = statement.query_map([agent.name()], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn materialization_sessions(&self) -> Result<Vec<MaterializationSession>> {
        let db = self.db.lock().unwrap();
        let mut statement =
            db.prepare_cached("SELECT id, leaf, replayable FROM sessions ORDER BY id")?;
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
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare_cached(
            "SELECT l.session, l.native_id, l.removable, l.writer_version FROM links l WHERE l.host = ?1 AND l.agent = ?2 AND NOT l.removed
             AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session))
             ORDER BY l.session, l.native_id",
        )?;
        let rows = statement.query_map(params![self.host, agent.name()], |row| {
            Ok(MaterializationLink {
                session: row.get(0)?,
                native_id: row.get(1)?,
                removable: row.get(2)?,
                writer_version: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn sessions_without_project(&self) -> Result<Vec<(String, Option<String>)>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare_cached(
            "SELECT id, cwd FROM sessions WHERE project_path IS NULL ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The session linked to the native session of `agent` whose id starts with `prefix`; fails when several sessions match.
    pub fn linked_session_by_prefix(&self, agent: Agent, prefix: &str) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare_cached(
            "SELECT DISTINCT l.session FROM links l WHERE l.agent = ?1 AND substr(l.native_id, 1, length(?2)) = ?2 AND NOT l.removed
             AND NOT EXISTS (SELECT 1 FROM links newer WHERE newer.host = l.host AND newer.agent = l.agent AND newer.native_id = l.native_id AND (newer.version, newer.name, newer.session) > (l.version, l.name, l.session))",
        )?;
        let rows = statement.query_map(params![agent.name(), prefix], |row| row.get(0))?;
        let sessions = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        unique(sessions, &format!("{agent}:{prefix}"))
    }

    /// The session whose id is `prefix` or uniquely starts with it; fails when several sessions match.
    pub fn resolve(&self, prefix: &str) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let mut statement =
            db.prepare_cached("SELECT id FROM sessions WHERE substr(id, 1, length(?1)) = ?1")?;
        let rows = statement.query_map([prefix], |row| row.get(0))?;
        let ids = rows.collect::<rusqlite::Result<Vec<String>>>()?;
        if ids.iter().any(|id| id == prefix) {
            return Ok(Some(prefix.to_owned()));
        }
        unique(ids, prefix)
    }

    pub(crate) fn title(&self, session: &str) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let title = db
            .prepare_cached("SELECT title FROM sessions WHERE id = ?1")?
            .query_row([session], |row| row.get(0))
            .optional()?;
        Ok(title.flatten())
    }

    /// Sessions from the project directory `cwd` and linked to `agent`, each filter applying only when given, most recently updated first.
    pub fn list(&self, cwd: Option<&str>, agent: Option<Agent>) -> Result<Vec<SessionSummary>> {
        let identity = cwd.and_then(|cwd| identify(Path::new(cwd)));
        let db = self.db.lock().unwrap();
        let mut statement = db.prepare_cached(
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
}

fn load(store: &Store, pending: Pending) -> Result<Loaded> {
    let mut segments = Vec::with_capacity(pending.segments.len());
    for name in &pending.segments {
        let events = store
            .segment(&pending.id, name)?
            .into_iter()
            .map(|event| IndexedEvent {
                id: event.id,
                parent: event.parent,
                timestamp: event.timestamp,
                replayable: event.body.replayable(),
                reset: event.body.is_reset(),
            })
            .collect();
        segments.push((name.clone(), events));
    }
    let records = pending
        .records
        .as_ref()
        .map(|records| store.index_records(&pending.id, records))
        .transpose()?;

    Ok(Loaded {
        pending,
        segments,
        records,
    })
}

fn apply(transaction: &rusqlite::Transaction<'_>, loaded: Loaded) -> Result<()> {
    let Pending {
        id,
        rebuild,
        new_session,
        added,
        ..
    } = loaded.pending;
    if rebuild {
        remove_session(transaction, &id)?;
    }
    if new_session {
        transaction.prepare_cached("INSERT INTO sessions (id, updated, events, alternate_branches, replayable) VALUES (?1, '', 0, 0, 0)")?.execute([&id])?;
    }
    let events_changed = new_session || !loaded.segments.is_empty();
    for (name, events) in loaded.segments {
        let (version, _) = record_order(&name);
        for event in events {
            transaction.prepare_cached(
                "INSERT INTO events (session, id, version, name, parent, ts, replayable, reset) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT (session, id) DO UPDATE SET version = excluded.version, name = excluded.name, ts = excluded.ts WHERE (excluded.version, excluded.name) < (events.version, events.name)",
            )?.execute(params![id, event.id, version, name, event.parent, event.timestamp, event.replayable, event.reset])?;
        }
    }
    if let Some((meta, links)) = loaded.records {
        transaction.prepare_cached(
            "UPDATE sessions SET title = ?2, cwd = ?3, project_remote = ?4, project_root = ?5, project_path = ?6 WHERE id = ?1",
        )?.execute(params![id, meta.title, meta.cwd, meta.project.as_ref().and_then(|project| project.remote.as_deref()), meta.project.as_ref().and_then(|project| project.root.as_deref()), meta.project.as_ref().map(|project| project.path.as_str())])?;
        transaction
            .prepare_cached("DELETE FROM links WHERE session = ?1")?
            .execute([&id])?;
        for (name, link) in links {
            let (version, _) = record_order(&name);
            let removable = link.direction == Direction::Export
                && !link.continued
                && link.exported_digest.is_some();
            transaction.prepare_cached(
                "INSERT INTO links (session, name, version, host, agent, native_id, path, removed, removable, writer_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?.execute(params![id, name, version, link.host, link.agent.name(), link.native_id, link.path, link.removed, removable, link.writer_version])?;
        }
    }
    if events_changed {
        transaction.prepare_cached(
            "UPDATE sessions SET
                    updated = COALESCE((SELECT MAX(ts) FROM events WHERE session = ?1), ''),
                    events = (SELECT COUNT(*) FROM events WHERE session = ?1),
                    alternate_branches = (SELECT COUNT(*) > 1 FROM events e WHERE e.session = ?1 AND NOT EXISTS (SELECT 1 FROM events child WHERE child.session = e.session AND child.parent = e.id)),
                    leaf = (SELECT e.id FROM events e WHERE e.session = ?1 AND NOT EXISTS (SELECT 1 FROM events child WHERE child.session = e.session AND child.parent = e.id) ORDER BY e.ts DESC, e.id DESC LIMIT 1)
                 WHERE id = ?1",
        )?.execute([&id])?;
        transaction.prepare_cached(
            "WITH RECURSIVE branch(id, parent, reset, replayable) AS (
                     SELECT e.id, e.parent, e.reset, e.replayable FROM events e JOIN sessions s ON s.id = e.session AND s.leaf = e.id WHERE e.session = ?1
                     UNION
                     SELECT e.id, e.parent, e.reset, e.replayable FROM branch b JOIN events e ON e.session = ?1 AND e.id = b.parent WHERE NOT b.reset
                 )
                 UPDATE sessions SET replayable = EXISTS (SELECT 1 FROM branch WHERE replayable) WHERE id = ?1",
        )?.execute([&id])?;
    }
    for (kind, name) in added {
        transaction
            .prepare_cached("INSERT INTO files (session, kind, name) VALUES (?1, ?2, ?3)")?
            .execute(params![id, kind, name])?;
    }

    Ok(())
}

fn unique(mut matches: Vec<String>, query: &str) -> Result<Option<String>> {
    match matches.len() {
        0 | 1 => Ok(matches.pop()),
        count => bail!("`{query}` matches {count} sessions, give more characters"),
    }
}

fn remove_session(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM events WHERE session = ?1",
        "DELETE FROM links WHERE session = ?1",
        "DELETE FROM files WHERE session = ?1",
        "DELETE FROM sessions WHERE id = ?1",
    ] {
        transaction.prepare_cached(sql)?.execute([id])?;
    }
    Ok(())
}
