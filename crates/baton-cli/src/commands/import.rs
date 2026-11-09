use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use anyhow::Result;
use baton_adapters::Paseo;
use baton_core::{Agent, Baton, Direction, ImportOutcome};

#[derive(Default)]
struct Tally {
    created: usize,
    updated: usize,
    forked: usize,
    unchanged: usize,
}

// One unreadable session or agent store is reported and skipped, so it cannot block the others
pub(crate) fn run(baton: &mut Baton, agent: Option<Agent>, path: Option<&Path>) -> Result<()> {
    let agents = match agent {
        Some(agent) => vec![agent],
        None => Agent::ALL.to_vec(),
    };
    let mut tally = Tally::default();
    for agent in agents {
        if agent == Agent::Paseo {
            import_paseo(baton, path, &mut tally)?;
            continue;
        }
        let source = baton_adapters::source(agent)?;
        let sessions = match source.discover(path) {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!("baton: {agent}: {error:#}");
                continue;
            }
        };
        for native in sessions {
            let reference = format!("{agent}:{}", native.id);
            match baton.import(&*source, &native) {
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
                Err(error) => eprintln!("baton: {reference}: {error:#}"),
            }
        }
    }
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
    let mut providers = HashMap::new();
    baton.refresh()?;
    for agent in paseo.discover()? {
        if let Entry::Vacant(entry) = providers.entry(agent.provider) {
            let source = baton_adapters::source(agent.provider)?;
            let natives = source.discover(None)?;
            entry.insert((source, natives));
        }
        let (source, natives) = &providers[&agent.provider];
        let native = natives
            .iter()
            .find(|native| Paseo::handle(agent.provider, native) == agent.handle);
        let linked = if let Some(native) = native {
            let outcome = baton.import(&**source, native)?;
            match outcome {
                ImportOutcome::Created { session } => {
                    tally.created += 1;
                    Some((session, native.id.clone()))
                }
                ImportOutcome::Appended { session, .. } => {
                    tally.updated += 1;
                    Some((session, native.id.clone()))
                }
                ImportOutcome::Forked { session, .. } => {
                    tally.forked += 1;
                    Some((session, native.id.clone()))
                }
                ImportOutcome::Unchanged => {
                    tally.unchanged += 1;
                    baton
                        .index
                        .linked_session_any_host(Some(agent.provider), &native.id)?
                        .map(|session| (session, native.id.clone()))
                }
            }
        } else if matches!(agent.provider, Agent::Pi | Agent::Omp) {
            baton
                .index
                .linked_file(agent.provider, Path::new(&agent.handle))?
        } else {
            let session = baton
                .index
                .linked_session_any_host(Some(agent.provider), &agent.handle)?;
            let session = match session {
                Some(session) => Some(session),
                None => baton.index.linked_session_any_host(None, &agent.handle)?,
            };
            session.map(|session| (session, agent.handle.clone()))
        };
        if let Some((session, native_id)) = linked {
            let existing = baton.store.load(&session)?;
            let direction = existing
                .meta
                .links
                .iter()
                .find(|link| {
                    link.host == baton.host()
                        && link.agent == Agent::Paseo
                        && link.native_id == agent.id
                })
                .map_or(Direction::Import, |link| link.direction);
            let link = paseo.link(baton.host(), &agent, native_id, direction);
            baton.record_link(&session, link, agent.title.clone())?;
        }
    }
    baton.refresh()?;
    Ok(())
}
