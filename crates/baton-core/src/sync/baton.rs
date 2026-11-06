use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::{BaseDirs, ProjectDirs};

use crate::cache::Cursors;
use crate::model::Record;
use crate::project::{identify, resolve};
use crate::sync::step::{self, Step};
use crate::sync::{migration, session_id};
use crate::{
    Adapter, Agent, Cursor, Direction, Draft, Event, Export, ForkPoint, Index, Info, Link,
    NativeRead, NativeSession, Progress, Session, SessionMeta, Source, Store, replay,
};

/// The data directory with its caches, locked for the lifetime of the value.
pub struct Baton {
    /// The unified sessions.
    pub store: Store,
    /// The index of the unified sessions, up to date as of [`Baton::open`].
    pub index: Index,
    cursors: Cursors,
    _lock: File,
}

/// A native session written by [`Baton::export`].
pub struct Exported {
    /// The new native session.
    pub native: NativeSession,
    /// The project directory it was written for, where the agent should resume it.
    pub cwd: PathBuf,
}

/// The results of materializing one agent's sessions.
#[derive(Default)]
pub struct Materialized {
    /// Sessions written for the first time on this host.
    pub exported: usize,
    /// Sessions written again because their active branch changed.
    pub refreshed: usize,
    /// Obsolete, untouched exported native sessions removed.
    pub removed: usize,
    /// Sessions already present at their active leaf.
    pub up_to_date: usize,
    /// Sessions that could not be written or have nothing to replay.
    pub skipped: usize,
    /// Individual sessions or removals that could not be processed.
    pub warnings: Vec<String>,
}

/// What [`Baton::import`] did with a native session.
#[derive(Debug)]
pub enum ImportOutcome {
    /// Nothing new, or a native session without any conversation.
    Unchanged,
    /// A new unified session was created for the native session.
    Created {
        /// The new unified session.
        session: String,
    },
    /// New events were appended to the linked unified session.
    Appended {
        /// The unified session.
        session: String,
        /// How many events were appended.
        events: usize,
    },
    /// The native session continued from a point the unified session had already moved past, so the continuation went to a new session.
    Forked {
        /// The new unified session.
        session: String,
        /// The unified session it forked off.
        from: String,
        /// How many events the native session added after the fork point.
        events: usize,
    },
}

// What one read of a native session brought in
struct Incoming<'a> {
    agent: Agent,
    native: &'a NativeSession,
    info: Info,
    progress: Progress,
}

impl Incoming<'_> {
    fn title(&self) -> Option<String> {
        self.native
            .title
            .clone()
            .or_else(|| self.info.title.clone())
    }
}

struct Fork<'a> {
    from: &'a SessionMeta,
    base: Option<String>,
}

impl Baton {
    /// Opens the data directory, `BATON_HOME` or else the platform data directory, and brings the index in the cache directory, `BATON_CACHE_DIR` or else the platform cache directory, up to date.
    ///
    /// Files are written under the host name `BATON_HOST`, or else the machine's host name. Fails on a data directory in the old layout, which [`Baton::migrate`] converts. Blocks while another baton process uses the same cache directory.
    pub fn open() -> Result<Baton> {
        let (data, cache, lock) = locked_dirs()?;
        if !migration::old_sessions(&data)?.is_empty() {
            bail!("{} uses an old layout; run `baton migrate`", data.display());
        }
        let store = Store::new(data.clone(), host()?);
        let mut index = Index::open(&cache, &data, store.host())?;
        index.refresh(&store)?;
        let cursors = Cursors::open(&cache, &data)?;
        Ok(Baton {
            store,
            index,
            cursors,
            _lock: lock,
        })
    }

    /// Converts the sessions of a data directory in the old layout, `events.jsonl` and `meta.json`, to the current one in place, and returns how many it converted.
    ///
    /// Each old session is removed only once its new session is fully written, so an interrupted migration picks up where it stopped when run again. The index is cleared and rebuilt by the next [`Baton::open`].
    pub fn migrate() -> Result<usize> {
        let (data, cache, _lock) = locked_dirs()?;
        let store = Store::new(data.clone(), host()?);
        let migrated = migration::run(&data, &store)?;
        for name in ["index.db", "index.db-wal", "index.db-shm"] {
            let path = cache.join(name);
            match fs::remove_file(&path) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    return Err(error).with_context(|| format!("removing {}", path.display()));
                }
                _ => {}
            }
        }
        Ok(migrated)
    }

    /// Updates the index after changes made during this process.
    pub fn refresh(&mut self) -> Result<()> {
        self.index.refresh(&self.store)
    }

    /// Adds project identities to older sessions whose recorded directories exist here.
    pub fn backfill_projects(&mut self) -> Result<()> {
        for (id, cwd) in self.index.sessions_without_project()? {
            if let Some(project) = cwd.as_deref().and_then(|cwd| identify(Path::new(cwd))) {
                self.store.record(
                    &id,
                    &Record {
                        project: Some(project),
                        ..Record::default()
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Writes each session's active branch to an agent and removes obsolete untouched exports when possible.
    pub fn materialize(
        &mut self,
        adapter: &(impl Adapter + ?Sized),
        natives: &[NativeSession],
    ) -> Result<Materialized> {
        let mut result = Materialized::default();
        let natives: HashMap<&str, &NativeSession> = natives
            .iter()
            .map(|native| (native.id.as_str(), native))
            .collect();
        let agent = adapter.agent();
        let indexed_links = self.index.materialization_links(agent)?;
        let mut links: HashMap<&str, Vec<_>> = HashMap::new();
        for link in &indexed_links {
            if let Some(native) = natives.get(link.native_id.as_str()) {
                links
                    .entry(&link.session)
                    .or_default()
                    .push((link, *native));
            }
        }
        for indexed in self.index.materialization_sessions()? {
            let id = indexed.id;
            if !indexed.replayable {
                result.skipped += 1;
                result.warnings.push(format!("{id}: nothing to replay"));
                continue;
            }
            let mut linked = Vec::new();
            for (link, native) in links.remove(id.as_str()).unwrap_or_default() {
                let cursor = self.cursors.get(agent, &link.native_id)?;
                let current = cursor.as_ref().is_some_and(|cursor| {
                    cursor.session == id
                        && cursor.path == native.path
                        && cursor.stamp == native.stamp
                        && cursor.event == indexed.leaf
                });
                linked.push((link, native, current));
            }
            let up_to_date = linked.iter().any(|(_, _, current)| *current);
            if up_to_date
                && !linked
                    .iter()
                    .any(|(link, _, current)| !current && link.removable)
            {
                result.up_to_date += 1;
                continue;
            }
            let session = self.store.load(&id)?;
            let path = session.path();
            let leaf = path.last().map(|event| event.id.as_str());
            let events = replay::events(&path);
            if !up_to_date {
                let cwd = resolve(session.meta.project.as_ref(), session.meta.cwd.as_deref())
                    .or_else(|| session.meta.cwd.as_ref().map(PathBuf::from))
                    .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
                    .context("finding the home directory")?;
                if let Err(error) = self.write_export(&session, adapter, cwd, &events, leaf) {
                    result.skipped += 1;
                    result.warnings.push(format!("{id}: {error:#}"));
                    continue;
                }
                if linked.is_empty() {
                    result.exported += 1;
                } else {
                    result.refreshed += 1;
                }
            } else {
                result.up_to_date += 1;
            }
            for (link, native, current) in linked {
                if current || !link.removable {
                    continue;
                }
                let link = session
                    .meta
                    .links
                    .iter()
                    .find(|candidate| candidate.is_for(self.store.host(), agent, &link.native_id))
                    .expect("indexed native link exists");
                let expected = link
                    .exported_digest
                    .as_ref()
                    .expect("removable native link has an export digest");
                let read = match adapter.read(native, None, &self.store.blobs()) {
                    Ok(read) => read,
                    Err(error) => {
                        result.warnings.push(format!("{}: {error:#}", native.id));
                        continue;
                    }
                };
                if replay::digest(read.drafts.iter().map(|draft| &draft.body)) != *expected {
                    continue;
                }
                match adapter.remove(native) {
                    Ok(()) => {
                        let mut removed = link.clone();
                        removed.removed = true;
                        self.store.record(
                            &id,
                            &Record {
                                link: Some(removed),
                                ..Record::default()
                            },
                        )?;
                        result.removed += 1;
                    }
                    Err(error) => result.warnings.push(format!("{}: {error:#}", native.id)),
                }
            }
        }
        Ok(result)
    }

    /// Brings a native session into its linked unified session, or into a new one when it has no link yet.
    ///
    /// Unchanged sessions are skipped and grown files are read incrementally. A native session the unified session was imported from is matched by event ids; one it was exported to is matched by [`replay::digest`] over the exported events. When the unified session moved on since the native session last matched it, the native continuation becomes a fork.
    pub fn import(
        &mut self,
        source: &(impl Source + ?Sized),
        native: &NativeSession,
    ) -> Result<ImportOutcome> {
        let agent = source.agent();
        let linked = self.index.linked_session(agent, &native.id)?;
        let cursor = self
            .cursors
            .get(agent, &native.id)?
            .filter(|cursor| linked.as_deref() == Some(cursor.session.as_str()));
        if let Some(cursor) = &cursor
            && cursor.path == native.path
            && cursor.stamp == native.stamp
        {
            if let Some(title) = &native.title
                && self.index.title(&cursor.session)?.as_ref() != Some(title)
            {
                let record = Record {
                    title: Some(title.clone()),
                    ..Record::default()
                };
                self.store.record(&cursor.session, &record)?;
            }
            return Ok(ImportOutcome::Unchanged);
        }
        let NativeRead {
            info,
            drafts,
            continued,
            progress,
        } = source.read(native, cursor.as_ref(), &self.store.blobs())?;
        let incoming = Incoming {
            agent,
            native,
            info,
            progress,
        };
        let Some(session_id) = linked else {
            if drafts.is_empty() {
                return Ok(ImportOutcome::Unchanged);
            }
            let events = Event::chain(drafts, None, agent, &native.id);
            let session = self.publish(&incoming, events, Direction::Import, None)?;
            return Ok(ImportOutcome::Created { session });
        };
        let mut session = self.store.load(&session_id)?;
        let host = self.store.host();
        let link = session
            .meta
            .links
            .iter()
            .find(|link| link.is_for(host, agent, &native.id))
            .cloned()
            .with_context(|| {
                format!("session {session_id} no longer links {agent}:{}", native.id)
            })?;
        let step = if continued {
            step::continuation(&session, cursor.and_then(|cursor| cursor.event), drafts)
        } else {
            match link.direction {
                Direction::Import => step::reimport(&session, cursor.as_ref(), drafts),
                Direction::Export => step::backflow(
                    &session,
                    link.exported_events,
                    cursor
                        .as_ref()
                        .and_then(|cursor| cursor.event.as_deref())
                        .or(link.exported_leaf.as_deref()),
                    drafts,
                ),
            }
        };
        if link.direction == Direction::Export
            && matches!(&step, Step::Append { .. } | Step::Fork { .. })
        {
            let link = session
                .meta
                .links
                .iter_mut()
                .find(|link| link.is_for(host, agent, &native.id))
                .expect("linked native session exists");
            if !link.continued {
                link.continued = true;
                self.store.record(
                    &session_id,
                    &Record {
                        link: Some(link.clone()),
                        ..Record::default()
                    },
                )?;
            }
        }
        match step {
            Step::Synced(event) => {
                self.mark_synced(&session, &incoming, event)?;
                Ok(ImportOutcome::Unchanged)
            }
            Step::Append { base, drafts } => self.append(session, &incoming, base, drafts),
            Step::Fork { base, drafts } => {
                self.fork(session, &incoming, link.direction, base, drafts)
            }
        }
    }

    /// Writes the active branch of a session as a new native session of the adapter's agent and links the two.
    ///
    /// The native session goes into `cwd`, or into the session's resolved local directory, or else into the current directory.
    pub fn export(
        &mut self,
        session_id: &str,
        adapter: &(impl Adapter + ?Sized),
        cwd: Option<&Path>,
    ) -> Result<Exported> {
        let session = self.store.load(session_id)?;
        let cwd = match cwd {
            Some(cwd) => cwd.to_path_buf(),
            None => match resolve(session.meta.project.as_ref(), session.meta.cwd.as_deref()) {
                Some(cwd) => cwd,
                None => env::current_dir()?,
            },
        };
        let path = session.path();
        let events = replay::events(&path);
        if events.is_empty() {
            bail!("session {session_id} has nothing to replay");
        }
        self.write_export(
            &session,
            adapter,
            cwd,
            &events,
            path.last().map(|event| event.id.as_str()),
        )
    }

    fn write_export(
        &self,
        session: &Session,
        adapter: &(impl Adapter + ?Sized),
        cwd: PathBuf,
        events: &[&Event],
        leaf: Option<&str>,
    ) -> Result<Exported> {
        let export = Export {
            title: session.meta.title.as_deref(),
            cwd: &cwd,
            created: &session.meta.created,
            events,
        };
        let native = adapter.write(&export, &self.store.blobs())?;
        let read = adapter.read(&native, None, &self.store.blobs())?;
        let exported_digest = replay::digest(read.drafts.iter().map(|draft| &draft.body));
        let record = Record {
            link: Some(Link {
                host: self.store.host().to_owned(),
                agent: adapter.agent(),
                native_id: native.id.clone(),
                path: native.path.to_string_lossy().into_owned(),
                exported_events: events.len(),
                exported_leaf: leaf.map(str::to_owned),
                exported_digest: Some(exported_digest),
                continued: false,
                removed: false,
                direction: Direction::Export,
            }),
            ..Record::default()
        };
        self.store.record(&session.meta.id, &record)?;
        Ok(Exported { native, cwd })
    }

    fn append(
        &mut self,
        mut session: Session,
        incoming: &Incoming,
        base: Option<String>,
        drafts: Vec<Draft>,
    ) -> Result<ImportOutcome> {
        let chained = Event::chain(drafts, base, incoming.agent, &incoming.native.id);
        // The native session's own last event, which need not be the newest leaf of the unified session
        let reached = chained.last().map(|event| event.id.clone());
        let added: Vec<Event> = chained
            .into_iter()
            .filter(|event| !session.contains(&event.id))
            .collect();
        let events = added.len();
        self.store.append(&session.meta.id, &added)?;
        session.extend(added);
        self.mark_synced(&session, incoming, reached)?;
        if events == 0 {
            return Ok(ImportOutcome::Unchanged);
        }
        Ok(ImportOutcome::Appended {
            session: session.meta.id,
            events,
        })
    }

    // Continues the drafts in a new session holding the history up to `base`; its link record is newer, so the link moves there
    fn fork(
        &mut self,
        session: Session,
        incoming: &Incoming,
        direction: Direction,
        base: Option<String>,
        drafts: Vec<Draft>,
    ) -> Result<ImportOutcome> {
        let mut events: Vec<Event> = match &base {
            Some(base) => session.path_to(base).into_iter().cloned().collect(),
            None => Vec::new(),
        };
        let added = Event::chain(drafts, base.clone(), incoming.agent, &incoming.native.id);
        let count = added.len();
        events.extend(added);
        // Without a shared base the fork copies the native session, so later reads align by id
        let direction = if base.is_some() {
            direction
        } else {
            Direction::Import
        };
        let fork = Fork {
            from: &session.meta,
            base,
        };
        let forked = self.publish(incoming, events, direction, Some(fork))?;
        Ok(ImportOutcome::Forked {
            session: forked,
            from: session.meta.id,
            events: count,
        })
    }

    // Stores a linear session that links the native session it came from, under an id every host derives alike
    fn publish(
        &mut self,
        incoming: &Incoming,
        events: Vec<Event>,
        direction: Direction,
        fork: Option<Fork>,
    ) -> Result<String> {
        let path: Vec<&Event> = events.iter().collect();
        let from = fork.as_ref().map(|fork| fork.from);
        let id = match &fork {
            Some(fork) => session_id::forked(
                &fork.from.id,
                fork.base.as_deref(),
                incoming.agent,
                &incoming.native.id,
            ),
            None => session_id::imported(incoming.agent, &incoming.native.id),
        };
        let parent = fork.map(|fork| ForkPoint {
            session: fork.from.id.clone(),
            event: fork.base,
        });
        let meta = SessionMeta {
            id,
            title: incoming
                .title()
                .or_else(|| from.and_then(|from| from.title.clone())),
            cwd: incoming
                .info
                .cwd
                .clone()
                .or_else(|| from.and_then(|from| from.cwd.clone())),
            project: incoming
                .info
                .cwd
                .as_deref()
                .and_then(|cwd| identify(Path::new(cwd)))
                .or_else(|| from.and_then(|from| from.project.clone())),
            created: String::new(),
            updated: String::new(),
            parent,
            links: vec![Link {
                host: self.store.host().to_owned(),
                agent: incoming.agent,
                native_id: incoming.native.id.clone(),
                path: incoming.native.path.to_string_lossy().into_owned(),
                exported_events: replay::events(&path).len(),
                exported_leaf: None,
                exported_digest: None,
                continued: direction == Direction::Export,
                removed: false,
                direction,
            }],
        };
        self.store
            .create(&meta, &events, incoming.info.created.as_deref())?;
        let leaf = events.last().map(|event| event.id.clone());
        self.save_cursor(incoming, leaf, &meta.id)?;
        Ok(meta.id)
    }

    fn mark_synced(
        &mut self,
        session: &Session,
        incoming: &Incoming,
        event: Option<String>,
    ) -> Result<()> {
        let exported = match event.as_deref() {
            Some(event) => replay::events(&session.path_to(event)).len(),
            None => 0,
        };
        let host = self.store.host();
        let mut link = session
            .meta
            .links
            .iter()
            .find(|link| link.is_for(host, incoming.agent, &incoming.native.id))
            .cloned()
            .expect("imports only sync sessions that link the native session");
        link.path = incoming.native.path.to_string_lossy().into_owned();
        link.exported_events = exported;
        let title = incoming
            .title()
            .filter(|title| session.meta.title.as_ref() != Some(title));
        let link = (!session.meta.links.contains(&link)).then_some(link);
        if title.is_some() || link.is_some() {
            let record = Record {
                title,
                link,
                ..Record::default()
            };
            self.store.record(&session.meta.id, &record)?;
        }
        self.save_cursor(incoming, event, &session.meta.id)
    }

    fn save_cursor(&self, incoming: &Incoming, event: Option<String>, session: &str) -> Result<()> {
        let cursor = Cursor {
            path: incoming.native.path.clone(),
            stamp: incoming.native.stamp,
            progress: incoming.progress.clone(),
            event,
            session: session.to_owned(),
        };
        self.cursors
            .save(incoming.agent, &incoming.native.id, &cursor)
    }
}

// An override skips the platform lookup, which fails on Windows when `USERPROFILE` names a profile without `AppData`
fn env_dir_or(name: &str, platform_dir: fn(&ProjectDirs) -> &Path) -> Result<PathBuf> {
    if let Some(dir) = env::var_os(name).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let dirs = ProjectDirs::from("", "", "baton").context("cannot locate the home directory")?;
    Ok(platform_dir(&dirs).to_path_buf())
}

// The data and cache directories, created when missing, and the lock on the cache directory
fn locked_dirs() -> Result<(PathBuf, PathBuf, File)> {
    let data = env_dir_or("BATON_HOME", ProjectDirs::data_dir)?;
    let cache = env_dir_or("BATON_CACHE_DIR", ProjectDirs::cache_dir)?;
    fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join("lock"))?;
    lock.lock()?;
    Ok((data, cache, lock))
}

// The host name that starts the names of the files this machine writes
fn host() -> Result<String> {
    let host = match env::var_os("BATON_HOST") {
        Some(host) => host.into_string(),
        None => hostname::get()?.into_string(),
    }
    .map_err(|host| anyhow!("host name {} is not valid Unicode", host.display()))?;
    if host.is_empty() || host.contains('/') {
        bail!("host name {host:?} cannot start a file name; set BATON_HOST");
    }
    Ok(host)
}
