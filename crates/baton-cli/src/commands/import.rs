use std::path::Path;

use anyhow::Result;
use baton_core::{Agent, Baton, ImportOutcome};

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
        let adapter = baton_adapters::adapter(agent)?;
        let sessions = match adapter.discover(path) {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!("baton: {agent}: {error:#}");
                continue;
            }
        };
        for native in sessions {
            let source = format!("{agent}:{}", native.id);
            match baton.import(adapter.as_ref(), &native) {
                Ok(ImportOutcome::Created { session }) => {
                    tally.created += 1;
                    println!("new     {session}  {source}");
                }
                Ok(ImportOutcome::Appended { session, events }) => {
                    tally.updated += 1;
                    println!("update  {session}  +{events}  {source}");
                }
                Ok(ImportOutcome::Forked {
                    session,
                    from,
                    events,
                }) => {
                    tally.forked += 1;
                    println!("fork    {session}  from {from}  +{events}  {source}");
                }
                Ok(ImportOutcome::Unchanged) => tally.unchanged += 1,
                Err(error) => eprintln!("baton: {source}: {error:#}"),
            }
        }
    }
    println!(
        "{} new, {} updated, {} forked, {} unchanged",
        tally.created, tally.updated, tally.forked, tally.unchanged
    );
    Ok(())
}
