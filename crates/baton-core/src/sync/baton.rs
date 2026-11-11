use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::ops::AddAssign;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::{BaseDirs, ProjectDirs};
use rayon::prelude::*;
use uuid::Uuid;

use crate::cache::{Cursors, MaterializationLink, MaterializationSession};
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
    _locks: [File; 2],
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

/// Counts sessions changed by a migration
pub struct Migrated {
    /// Sessions converted from the old layout
    pub converted: usize,
    /// Sessions with rewritten call identifiers
    pub rewritten: usize,
    /// Sessions with corrected project remotes
    pub corrected: usize,
}

impl AddAssign for Materialized {
    fn add_assign(&mut self, other: Self) {
        self.exported += other.exported;
        self.refreshed += other.refreshed;
        self.removed += other.removed;
        self.up_to_date += other.up_to_date;
        self.skipped += other.skipped;
        self.warnings.extend(other.warnings);
    }
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
    continued: bool,
}

impl Incoming<'_> {
    fn title(&self) -> Option<String> {
        self.native
            .title
            .clone()
            .or_else(|| self.info.title.clone())
    }

    fn cursor(&self, event: Option<String>, session: &str) -> Cursor {
        Cursor {
            path: self.native.path.clone(),
            stamp: self.native.stamp,
            progress: self.progress.clone(),
            event,
            session: session.to_owned(),
        }
    }
}

struct Fork<'a> {
    from: &'a SessionMeta,
    base: Option<String>,
}

struct PendingExport {
    exported: Exported,
    record: Option<Record>,
    cursor: Cursor,
}

#[derive(Default)]
struct PendingMaterialization<'a> {
    export: Option<PendingExport>,
    obsolete: Vec<(Link, &'a NativeSession)>,
    result: Materialized,
}

impl Baton {
    /// Opens the data directory, `BATON_HOME` or else the platform data directory, and brings the index in the cache directory, `BATON_CACHE_DIR` or else the platform cache directory, up to date.
    ///
    /// Files are written under the host name `BATON_HOST`, or else the machine's host name. Fails on a data directory written in an older format, which [`Baton::migrate`] upgrades, or a newer format. Blocks while another baton process uses the same data or cache directory
    pub fn open() -> Result<Baton> {
        let (data, cache, locks) = locked_dirs()?;
        migration::ensure_current_format(&data)?;
        let store = Store::new(data.clone(), host()?);
        let mut index = Index::open(&cache, &data, store.host())?;
        index.refresh(&store)?;
        let cursors = Cursors::open(&cache, &data)?;
        Ok(Baton {
            store,
            index,
            cursors,
            _locks: locks,
        })
    }

    /// Upgrades a data directory written in an older format in place, finishes an interrupted run when run again, and lets the next [`Baton::open`] rebuild the index
    pub fn migrate() -> Result<Migrated> {
        let (data, cache, _locks) = locked_dirs()?;
        let store = Store::new(data.clone(), host()?);

        migration::migrate(&data, &cache, &store)
    }

    /// Updates the index after changes made during this process.
    pub fn refresh(&mut self) -> Result<()> {
        self.index.refresh(&self.store)
    }

    /// The host name used for this machine's links.
    pub fn host(&self) -> &str {
        self.store.host()
    }

    /// Records a changed registry link and supplies a title only when the session has none.
    pub fn record_link(&self, session_id: &str, link: Link, title: Option<String>) -> Result<()> {
        let meta = self.store.record_meta(session_id)?;
        let changed = !meta.links.contains(&link);
        let title = title.filter(|_| meta.title.is_none());
        if changed || title.is_some() {
            self.store.record(
                session_id,
                &Record {
                    title,
                    link: changed.then_some(link),
                    ..Record::default()
                },
            )?;
        }
        Ok(())
    }

    /// Returns each session's latest active registry link for each provider agent
    pub fn registry_sessions(&self, agent: Agent) -> Result<Vec<(String, Link)>> {
        let mut indexed = self.index.registry_links(agent)?;
        indexed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut result = Vec::new();
        let mut providers = HashSet::new();
        for session in indexed.chunk_by(|a, b| a.0 == b.0) {
            let id = &session[0].0;
            let mut versions: HashMap<_, _> = self.store.link_versions(id)?.into_iter().collect();
            providers.clear();
            for (_, name) in session {
                let link = versions.remove(name).expect("indexed link exists");
                if providers.insert(link.provider.as_ref().map(|provider| provider.agent)) {
                    result.push((id.clone(), link));
                }
            }
        }

        Ok(result)
    }

    /// Adds project identities to older sessions whose recorded directories exist here.
    pub fn backfill_projects(&self) -> Result<()> {
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

    /// Writes each selected session's active branch to an agent and removes obsolete untouched exports except those in `protected`
    pub fn materialize(
        &self,
        adapter: &(impl Adapter + ?Sized),
        natives: &[NativeSession],
        sessions: Option<&HashSet<String>>,
        protected: &HashSet<String>,
    ) -> Result<Materialized> {
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
        let work: Vec<_> = self
            .index
            .materialization_sessions()?
            .into_iter()
            .filter(|indexed| sessions.is_none_or(|sessions| sessions.contains(&indexed.id)))
            .map(|indexed| {
                let linked = links.remove(indexed.id.as_str()).unwrap_or_default();

                (indexed, linked)
            })
            .collect();
        let prepared: Vec<_> = work
            .into_par_iter()
            .map(|(indexed, linked)| {
                let prepared = self.prepare_materialization(adapter, &indexed, linked, protected);

                (indexed.id, prepared)
            })
            .collect();
        let mut result = Materialized::default();
        let mut errors = Vec::new();
        let mut pending = Vec::new();
        for (id, preparation) in prepared {
            match preparation {
                Ok(preparation) => pending.push((id, preparation)),
                Err(error) => {
                    result.skipped += 1;
                    result.warnings.push(format!("{id}: {error:#}"));
                }
            }
        }
        pending
            .par_iter_mut()
            .for_each(|(_, preparation)| self.remove_obsolete(adapter, preparation));
        adapter.flush().with_context(|| {
            format!("saving {agent} materialization before recording links and removals")
        })?;
        for (id, preparation) in pending {
            if let Some(export) = preparation.export
                && let Err(error) = self.publish_export(agent, &export)
            {
                errors.push(format!("{error:#}"));
            }
            for (link, native) in preparation.obsolete {
                let record = Record {
                    link: Some(link),
                    ..Record::default()
                };
                if let Err(error) = self.store.record(&id, &record) {
                    errors.push(format!(
                        "removed {agent}:{} but could not record its removal in {id}: {error:#}",
                        native.id
                    ));
                }
            }
            result += preparation.result;
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }

        Ok(result)
    }

    fn prepare_materialization<'a>(
        &self,
        adapter: &(impl Adapter + ?Sized),
        indexed: &MaterializationSession,
        linked: Vec<(&MaterializationLink, &'a NativeSession)>,
        protected: &HashSet<String>,
    ) -> Result<PendingMaterialization<'a>> {
        let mut pending = PendingMaterialization::default();
        let agent = adapter.agent();
        let id = &indexed.id;
        if !indexed.replayable {
            pending.result.skipped += 1;
            pending
                .result
                .warnings
                .push(format!("{id}: nothing to replay"));

            return Ok(pending);
        }
        let had_native = !linked.is_empty();
        let mut up_to_date = false;
        let mut obsolete = Vec::new();
        for (link, native) in linked {
            let current = self
                .cursors
                .get(agent, &link.native_id)?
                .is_some_and(|cursor| {
                    cursor.session == *id
                        && cursor.path == native.path
                        && cursor.stamp == native.stamp
                        && cursor.event == indexed.leaf
                });
            up_to_date |= current;
            if !current && link.removable && !protected.contains(&native.id) {
                obsolete.push((link, native));
            }
        }
        if up_to_date {
            pending.result.up_to_date += 1;
            if obsolete.is_empty() {
                return Ok(pending);
            }
        }

        let session = self.store.load(id)?;
        if !up_to_date {
            let path = session.path();
            let leaf = path.last().map(|event| event.id.as_str());
            let events = replay::events(&path);
            let seed = session_id::exported(id, self.store.host(), agent, leaf);
            pending.export = Some(self.prepare_export(
                &session,
                adapter,
                materialization_cwd(&session)?,
                &events,
                leaf,
                Some(seed),
            )?);
            if had_native {
                pending.result.refreshed += 1;
            } else {
                pending.result.exported += 1;
            }
        }
        for (link, native) in obsolete {
            let link = session
                .meta
                .links
                .iter()
                .find(|candidate| candidate.is_for(self.store.host(), agent, &link.native_id))
                .expect("indexed native link exists");
            pending.obsolete.push((link.clone(), native));
        }

        Ok(pending)
    }

    fn remove_obsolete(
        &self,
        adapter: &(impl Adapter + ?Sized),
        pending: &mut PendingMaterialization,
    ) {
        let result = &mut pending.result;
        pending.obsolete.retain_mut(|(link, native)| {
            let removal = self
                .untouched_export(adapter, link, native)
                .and_then(|untouched| {
                    if untouched {
                        adapter.remove(native)?;
                    }
                    Ok(untouched)
                });
            match removal {
                Ok(true) => {
                    link.removed = true;
                    result.removed += 1;

                    true
                }
                Ok(false) => false,
                Err(error) => {
                    result.warnings.push(format!("{}: {error:#}", native.id));
                    false
                }
            }
        });
    }

    /// Checks whether a local export still matches what Baton wrote. The caller must check that the session still owns the link before removing anything
    pub fn untouched_export(
        &self,
        adapter: &(impl Adapter + ?Sized),
        link: &Link,
        native: &NativeSession,
    ) -> Result<bool> {
        let Some(expected) = &link.exported_digest else {
            return Ok(false);
        };
        if !link.is_for(self.host(), adapter.agent(), &native.id)
            || link.direction != Direction::Export
            || link.continued
            || link.removed
        {
            return Ok(false);
        }
        let read = adapter.read(native, None, &self.store.blobs())?;

        Ok(replay::digest(read.drafts.iter().map(|draft| &draft.body)) == *expected)
    }

    /// Brings a native session into its linked unified session, or into a new one when it has no link yet.
    ///
    /// Unchanged sessions are skipped and grown files are read incrementally. A native session the unified session was imported from is matched by event ids; one it was exported to is matched by [`replay::digest`] over the exported events. When the unified session moved on since the native session last matched it, the native continuation becomes a fork.
    pub fn import(
        &self,
        source: &(impl Source + ?Sized),
        native: &NativeSession,
    ) -> Result<ImportOutcome> {
        let linked = self.index.linked_session(source.agent(), &native.id)?;
        self.import_linked(source, native, linked)
    }

    /// Imports native sessions in parallel, processing sessions linked to the same unified session one after another.
    pub fn import_all(
        &self,
        source: &(impl Source + ?Sized),
        natives: &[NativeSession],
    ) -> Vec<Result<ImportOutcome>> {
        let mut groups: Vec<Vec<_>> = Vec::new();
        let mut linked_groups = HashMap::new();
        let mut outcomes = Vec::new();
        for (position, native) in natives.iter().enumerate() {
            match self.index.linked_session(source.agent(), &native.id) {
                Ok(linked) => {
                    let group = match &linked {
                        Some(session) => {
                            *linked_groups.entry(session.clone()).or_insert(groups.len())
                        }
                        None => groups.len(),
                    };
                    if group == groups.len() {
                        groups.push(Vec::new());
                    }
                    groups[group].push((position, native, linked));
                }
                Err(error) => outcomes.push((position, Err(error))),
            }
        }
        outcomes.par_extend(groups.into_par_iter().flat_map_iter(|group| {
            group.into_iter().map(move |(position, native, linked)| {
                (position, self.import_linked(source, native, linked))
            })
        }));
        outcomes.sort_unstable_by_key(|(position, _)| *position);

        outcomes.into_iter().map(|(_, outcome)| outcome).collect()
    }

    fn import_linked(
        &self,
        source: &(impl Source + ?Sized),
        native: &NativeSession,
        linked: Option<String>,
    ) -> Result<ImportOutcome> {
        let agent = source.agent();
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
            continued,
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
        &self,
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
        let pending = self.prepare_export(
            &session,
            adapter,
            cwd,
            &events,
            path.last().map(|event| event.id.as_str()),
            None,
        )?;
        let agent = adapter.agent();
        adapter.flush().with_context(|| {
            format!(
                "saving {agent}:{} before linking it to {session_id}",
                pending.exported.native.id
            )
        })?;
        self.publish_export(agent, &pending)?;

        Ok(pending.exported)
    }

    fn prepare_export(
        &self,
        session: &Session,
        adapter: &(impl Adapter + ?Sized),
        cwd: PathBuf,
        events: &[&Event],
        leaf: Option<&str>,
        seed: Option<Uuid>,
    ) -> Result<PendingExport> {
        if events.is_empty() {
            bail!("session {} has nothing to replay", session.meta.id);
        }
        let export = Export {
            title: session.meta.title.as_deref(),
            cwd: &cwd,
            created: &session.meta.created,
            events,
            seed,
        };
        let blobs = self.store.blobs();
        let agent = adapter.agent();
        let native = adapter.write(&export, &blobs)?;
        let read = adapter
            .read(&native, None, &blobs)
            .with_context(|| format!("reading back {agent}:{} after export", native.id));
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                return Err(match discard_export(adapter, &native) {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!("{cleanup:#}")),
                });
            }
        };
        let exported_digest = replay::digest(read.drafts.iter().map(|draft| &draft.body));
        let link = Link {
            host: self.store.host().to_owned(),
            agent,
            native_id: native.id.clone(),
            provider: None,
            path: native.path.to_string_lossy().into_owned(),
            exported_events: events.len(),
            exported_leaf: leaf.map(str::to_owned),
            exported_digest: Some(exported_digest),
            continued: false,
            removed: false,
            direction: Direction::Export,
        };
        let incoming = Incoming {
            agent,
            native: &native,
            info: read.info,
            progress: read.progress,
            continued: false,
        };
        let title = incoming
            .title()
            .filter(|title| session.meta.title.as_ref() != Some(title));
        let link = (!session.meta.links.contains(&link)).then_some(link);
        let record = (title.is_some() || link.is_some()).then_some(Record {
            title,
            link,
            ..Record::default()
        });
        let cursor = incoming.cursor(leaf.map(str::to_owned), &session.meta.id);

        Ok(PendingExport {
            exported: Exported { native, cwd },
            record,
            cursor,
        })
    }

    fn publish_export(&self, agent: Agent, pending: &PendingExport) -> Result<()> {
        let native = &pending.exported.native;
        let session = &pending.cursor.session;
        if let Some(record) = &pending.record {
            self.store.record(session, record).with_context(|| {
                format!("saved {agent}:{} at {} but could not record its link to {session}; preserve any new native work and remove the unlinked export before retrying materialization", native.id, native.path.display())
            })?;
        }

        self.cursors.save(agent, &native.id, &pending.cursor).with_context(|| {
            format!("linked {agent}:{} to {session} but could not save its cursor; reimport the native session to rebuild the cursor", native.id)
        })
    }

    fn append(
        &self,
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
        &self,
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
        &self,
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
                provider: None,
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
        &self,
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
        let source_metadata = !incoming.continued && link.direction == Direction::Import;
        link.path = incoming.native.path.to_string_lossy().into_owned();
        link.exported_events = exported;
        let title = incoming
            .title()
            .filter(|title| session.meta.title.as_ref() != Some(title));
        let link = (!session.meta.links.contains(&link)).then_some(link);
        let cwd = incoming
            .info
            .cwd
            .as_ref()
            .filter(|cwd| source_metadata && session.meta.cwd.as_ref() != Some(*cwd));
        let created = incoming
            .info
            .created
            .as_ref()
            .filter(|created| source_metadata && session.meta.created != **created);
        if title.is_some() || link.is_some() || cwd.is_some() || created.is_some() {
            let record = Record {
                title,
                link,
                cwd: cwd.cloned(),
                project: cwd.and_then(|cwd| identify(Path::new(cwd))),
                created: created.cloned(),
                ..Record::default()
            };
            self.store.record(&session.meta.id, &record)?;
        }
        self.save_cursor(incoming, event, &session.meta.id)
    }

    fn save_cursor(&self, incoming: &Incoming, event: Option<String>, session: &str) -> Result<()> {
        self.cursors.save(
            incoming.agent,
            &incoming.native.id,
            &incoming.cursor(event, session),
        )
    }
}

fn discard_export(adapter: &(impl Adapter + ?Sized), native: &NativeSession) -> Result<()> {
    adapter.remove(native).with_context(|| {
        format!(
            "could not clean up newly written {}:{} at {}",
            adapter.agent(),
            native.id,
            native.path.display()
        )
    })
}

/// Resolves the directory used when materializing a session on this host.
pub fn materialization_cwd(session: &Session) -> Result<PathBuf> {
    resolve(session.meta.project.as_ref(), session.meta.cwd.as_deref())
        .or_else(|| session.meta.cwd.as_ref().map(PathBuf::from))
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
        .context("finding the home directory")
}

// An override skips the platform lookup, which fails on Windows when `USERPROFILE` names a profile without `AppData`
fn env_dir_or(name: &str, platform_dir: fn(&ProjectDirs) -> &Path) -> Result<PathBuf> {
    if let Some(dir) = env::var_os(name).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let dirs = ProjectDirs::from("", "", "baton").context("cannot locate the home directory")?;
    Ok(platform_dir(&dirs).to_path_buf())
}

fn locked_dirs() -> Result<(PathBuf, PathBuf, [File; 2])> {
    let data = env_dir_or("BATON_HOME", ProjectDirs::data_dir)?;
    let cache = env_dir_or("BATON_CACHE_DIR", ProjectDirs::cache_dir)?;
    fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    // Separate names allow a shared directory, and .tmp keeps the data lock out of the synced repository
    let data_lock = lock_file(&data.join(".lock.tmp"))?;
    let cache_lock = lock_file(&cache.join("lock"))?;

    Ok((data, cache, [data_lock, cache_lock]))
}

fn lock_file(path: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", path.display()))?;

    Ok(lock)
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
