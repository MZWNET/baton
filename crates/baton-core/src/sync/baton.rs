use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use uuid::Uuid;

use crate::cache::Cursors;
use crate::sync::step::{self, Step};
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
    /// Blocks while another baton process uses the same cache directory.
    pub fn open() -> Result<Baton> {
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
        let store = Store::new(data.clone());
        let mut index = Index::open(&cache, &data)?;
        index.refresh(&store)?;
        let cursors = Cursors::open(&cache, &data)?;
        Ok(Baton {
            store,
            index,
            cursors,
            _lock: lock,
        })
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
                let mut meta = self.store.meta(&cursor.session)?;
                meta.title = Some(title.clone());
                self.store.save_meta(&meta)?;
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
        let session = self.store.load(&session_id)?;
        let link = session
            .meta
            .links
            .iter()
            .find(|link| link.is_for(agent, &native.id))
            .cloned()
            .with_context(|| {
                format!("session {session_id} no longer links {agent}:{}", native.id)
            })?;
        let step = if continued {
            step::continuation(&session, cursor.and_then(|cursor| cursor.event), drafts)
        } else {
            match link.direction {
                Direction::Import => step::reimport(&session, cursor.as_ref(), drafts),
                Direction::Export => step::backflow(&session, link.exported_events, drafts),
            }
        };
        match step {
            Step::Synced(event) => {
                self.mark_synced(session, &incoming, event)?;
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
    /// The native session goes into `cwd`, or into the session's own directory when that still exists, or else into the current directory.
    pub fn export(
        &mut self,
        session_id: &str,
        adapter: &(impl Adapter + ?Sized),
        cwd: Option<&Path>,
    ) -> Result<Exported> {
        let mut session = self.store.load(session_id)?;
        let cwd = match cwd {
            Some(cwd) => cwd.to_path_buf(),
            None => match session.meta.cwd.as_deref().map(PathBuf::from) {
                Some(cwd) if cwd.is_dir() => cwd,
                _ => env::current_dir()?,
            },
        };
        let (native, exported) = {
            let path = session.path();
            let events = replay::events(&path);
            if events.is_empty() {
                bail!("session {session_id} has nothing to replay");
            }
            let export = Export {
                title: session.meta.title.as_deref(),
                cwd: &cwd,
                created: &session.meta.created,
                events: &events,
            };
            (adapter.write(&export, &self.store.blobs())?, events.len())
        };
        session.meta.links.push(Link {
            agent: adapter.agent(),
            native_id: native.id.clone(),
            path: native.path.to_string_lossy().into_owned(),
            exported_events: exported,
            direction: Direction::Export,
        });
        self.store.save_meta(&session.meta)?;
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
        let added: Vec<Event> = chained
            .into_iter()
            .filter(|event| !session.contains(&event.id))
            .collect();
        let events = added.len();
        self.store.append(&session.meta.id, &added)?;
        session.extend(added);
        let leaf = session.leaf().map(String::from);
        let id = session.meta.id.clone();
        self.mark_synced(session, incoming, leaf)?;
        if events == 0 {
            return Ok(ImportOutcome::Unchanged);
        }
        Ok(ImportOutcome::Appended {
            session: id,
            events,
        })
    }

    // Continues the drafts in a new session holding the history up to `base`, and moves the link there
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
        let mut from = session.meta;
        from.links
            .retain(|link| !link.is_for(incoming.agent, &incoming.native.id));
        self.store.save_meta(&from)?;
        Ok(ImportOutcome::Forked {
            session: forked,
            from: from.id,
            events: count,
        })
    }

    // Stores a new linear session that links only the native session it came from
    fn publish(
        &mut self,
        incoming: &Incoming,
        events: Vec<Event>,
        direction: Direction,
        fork: Option<Fork>,
    ) -> Result<String> {
        let path: Vec<&Event> = events.iter().collect();
        let from = fork.as_ref().map(|fork| fork.from);
        let parent = fork.map(|fork| ForkPoint {
            session: fork.from.id.clone(),
            event: fork.base,
        });
        let meta = SessionMeta {
            id: Uuid::now_v7().to_string(),
            title: incoming
                .title()
                .or_else(|| from.and_then(|from| from.title.clone())),
            cwd: incoming
                .info
                .cwd
                .clone()
                .or_else(|| from.and_then(|from| from.cwd.clone())),
            created: incoming
                .info
                .created
                .clone()
                .unwrap_or_else(|| events[0].timestamp.clone()),
            updated: events
                .iter()
                .map(|event| &event.timestamp)
                .max()
                .cloned()
                .unwrap_or_default(),
            parent,
            links: vec![Link {
                agent: incoming.agent,
                native_id: incoming.native.id.clone(),
                path: incoming.native.path.to_string_lossy().into_owned(),
                exported_events: replay::events(&path).len(),
                direction,
            }],
        };
        self.store.create(&meta, &events)?;
        let leaf = events.last().map(|event| event.id.clone());
        self.save_cursor(incoming, leaf, &meta.id)?;
        Ok(meta.id)
    }

    fn mark_synced(
        &mut self,
        mut session: Session,
        incoming: &Incoming,
        event: Option<String>,
    ) -> Result<()> {
        let exported = match event.as_deref() {
            Some(event) => replay::events(&session.path_to(event)).len(),
            None => 0,
        };
        let before = session.meta.clone();
        if let Some(title) = incoming.title() {
            session.meta.title = Some(title);
        }
        if let Some(last) = session.last()
            && last.timestamp > session.meta.updated
        {
            session.meta.updated = last.timestamp.clone();
        }
        let link = session
            .meta
            .links
            .iter_mut()
            .find(|link| link.is_for(incoming.agent, &incoming.native.id))
            .expect("imports only sync sessions that link the native session");
        link.path = incoming.native.path.to_string_lossy().into_owned();
        link.exported_events = exported;
        if session.meta != before {
            self.store.save_meta(&session.meta)?;
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
