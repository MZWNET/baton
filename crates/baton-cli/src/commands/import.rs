use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Display;
use std::fs;
use std::path::Path;

use anyhow::{Context, Error, Result, bail};
use baton_adapters::{Paseo, PaseoAgent};
use baton_core::{Agent, Baton, Direction, ImportOutcome, NativeSession, SessionMeta, Source};

#[derive(Default)]
pub(crate) struct Tally {
    created: usize,
    updated: usize,
    forked: usize,
    unchanged: usize,
    failed: usize,
}

impl Tally {
    fn fail(&mut self, subject: impl Display, error: &Error) {
        eprintln!("baton: {subject}: {error:#}");
        self.failed += 1;
    }
}

// One unreadable session or agent store is reported and skipped, so it cannot block the others, and fails the import once the rest is done
pub(crate) fn run(baton: &mut Baton, agent: Option<Agent>, path: Option<&Path>) -> Result<()> {
    // Several agents find nothing at a path that does not exist, which would hide a mistyped one
    if let Some(path) = path {
        fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    }
    let agents = match agent {
        Some(agent) => vec![agent],
        None => Agent::ALL.to_vec(),
    };
    let mut tally = Tally::default();
    for agent in agents {
        let imported = if agent == Agent::Paseo {
            import_paseo(baton, path, &mut tally)
        } else {
            import_store(baton, agent, path, &mut tally)
        };
        if let Err(error) = imported {
            tally.fail(agent, &error);
        }
    }
    finish(baton, &tally)?;
    match tally.failed {
        0 => Ok(()),
        failed if agent.is_none() && path.is_some() => bail!(
            "imports failed: {failed}; without --agent every agent reads the --path, so pass --agent when it belongs to one"
        ),
        failed => bail!("imports failed: {failed}"),
    }
}

fn import_store(baton: &Baton, agent: Agent, path: Option<&Path>, tally: &mut Tally) -> Result<()> {
    let source = baton_adapters::source(agent)?;
    let sessions = source.discover(path)?;
    import_sessions(baton, &*source, &sessions, tally);

    Ok(())
}

pub(crate) fn import_sessions(
    baton: &Baton,
    source: &dyn Source,
    sessions: &[NativeSession],
    tally: &mut Tally,
) {
    for (native, outcome) in sessions.iter().zip(baton.import_all(source, sessions)) {
        let reference = format!("{}:{}", source.agent(), native.id);
        match outcome {
            Ok(ImportOutcome::Created { session }) => {
                tally.created += 1;
                println!("new     {session}  {reference}");
            }
            Ok(ImportOutcome::Appended { session, events }) => {
                tally.updated += 1;
                println!("update  {session}  +{events}  {reference}");
            }
            Ok(ImportOutcome::Forked {
                session,
                from,
                events,
            }) => {
                tally.forked += 1;
                println!("fork    {session}  from {from}  +{events}  {reference}");
            }
            Ok(ImportOutcome::Unchanged) => tally.unchanged += 1,
            Err(error) => tally.fail(reference, &error),
        }
    }
}

pub(crate) fn finish(baton: &Baton, tally: &Tally) -> Result<()> {
    baton.backfill_projects()?;
    println!(
        "{} new, {} updated, {} forked, {} unchanged",
        tally.created, tally.updated, tally.forked, tally.unchanged
    );

    Ok(())
}

fn import_paseo(baton: &mut Baton, path: Option<&Path>, tally: &mut Tally) -> Result<()> {
    let paseo = Paseo::new(match path {
        Some(path) => path.to_path_buf(),
        None => Paseo::default_home()?,
    });
    let registrations = paseo.discover()?;
    import_registrations(baton, &paseo, &registrations, tally)
}

// The sessions of a Paseo provider, by the handle Paseo knows them by
struct Provider {
    source: Box<dyn Source>,
    natives: HashMap<String, NativeSession>,
}

impl Provider {
    fn discover(agent: Agent) -> Result<Provider> {
        let source = baton_adapters::source(agent)?;
        let mut natives = HashMap::new();
        for native in source.discover(None)? {
            natives
                .entry(Paseo::handle(agent, &native))
                .or_insert(native);
        }

        Ok(Provider { source, natives })
    }

    fn session(&self, handle: &str) -> Option<(&dyn Source, &NativeSession)> {
        let native = self.natives.get(handle)?;

        Some((self.source.as_ref(), native))
    }
}

// A provider that cannot be read is reported once, and its agents still link to sessions imported before
pub(crate) fn import_registrations(
    baton: &mut Baton,
    paseo: &Paseo,
    registrations: &[PaseoAgent],
    tally: &mut Tally,
) -> Result<()> {
    baton.refresh()?;
    let mut providers = HashMap::new();
    for registration in registrations {
        let provider = match providers.entry(registration.provider) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(match Provider::discover(registration.provider) {
                Ok(provider) => Some(provider),
                Err(error) => {
                    tally.fail(registration.provider, &error);
                    None
                }
            }),
        };
        let native = provider
            .as_ref()
            .and_then(|provider| provider.session(&registration.handle));
        if let Err(error) = import_wrapped(baton, paseo, registration, native, tally) {
            tally.fail(format_args!("paseo:{}", registration.id), &error);
        }
    }
    baton.refresh()
}

// Imports one Paseo agent, failing on any error, and returns the session it is linked to
pub(crate) fn import_registration(
    baton: &Baton,
    paseo: &Paseo,
    registration: &PaseoAgent,
) -> Result<Option<String>> {
    let provider = Provider::discover(registration.provider)?;
    let native = provider.session(&registration.handle);
    import_wrapped(baton, paseo, registration, native, &mut Tally::default())
}

// Imports the provider session a Paseo agent wraps, or finds the session holding it when the provider session is gone, and links the agent there
fn import_wrapped(
    baton: &Baton,
    paseo: &Paseo,
    registration: &PaseoAgent,
    native: Option<(&dyn Source, &NativeSession)>,
    tally: &mut Tally,
) -> Result<Option<String>> {
    let provider = registration.provider;
    let linked = match native {
        Some((source, native)) => {
            let session = match baton.import(source, native)? {
                ImportOutcome::Created { session } => {
                    tally.created += 1;
                    Some(session)
                }
                ImportOutcome::Appended { session, .. } => {
                    tally.updated += 1;
                    Some(session)
                }
                ImportOutcome::Forked { session, .. } => {
                    tally.forked += 1;
                    Some(session)
                }
                ImportOutcome::Unchanged => {
                    tally.unchanged += 1;
                    baton
                        .index
                        .linked_session_any_host(Some(provider), &native.id)?
                }
            };
            session.map(|session| (session, native.id.clone()))
        }
        None if matches!(provider, Agent::Pi | Agent::Omp) => baton
            .index
            .linked_file(provider, Path::new(&registration.handle))?,
        None => {
            let session = match baton
                .index
                .linked_session_any_host(Some(provider), &registration.handle)?
            {
                Some(session) => Some(session),
                None => baton
                    .index
                    .linked_session_any_host(None, &registration.handle)?,
            };
            session.map(|session| (session, registration.handle.clone()))
        }
    };
    let Some((session, native_id)) = linked else {
        return Ok(None);
    };
    let meta = baton.store.load(&session)?.meta;
    link_registration(
        baton,
        paseo,
        &meta,
        registration,
        native_id,
        Direction::Import,
        registration.title.clone(),
    )?;

    Ok(Some(session))
}

// A session keeps the direction it already links a Paseo agent with, so a registration baton made stays one it may delete
pub(crate) fn link_registration(
    baton: &Baton,
    paseo: &Paseo,
    session: &SessionMeta,
    registration: &PaseoAgent,
    native_id: String,
    direction: Direction,
    title: Option<String>,
) -> Result<()> {
    let direction = session
        .links
        .iter()
        .find(|link| {
            link.host == baton.host()
                && link.agent == Agent::Paseo
                && link.native_id == registration.id
        })
        .map_or(direction, |link| link.direction);
    baton.record_link(
        &session.id,
        paseo.link(baton.host(), registration, native_id, direction),
        title,
    )
}
