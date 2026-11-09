use std::collections::{HashMap, HashSet};

use anyhow::Result;
use baton_adapters::Paseo;
use baton_core::{Agent, Baton, Direction, Exported, Materialized, materialization_cwd};

use super::{export, import};

pub(crate) fn run(baton: &mut Baton, agents: &[Agent]) -> Result<()> {
    for (position, &agent) in agents.iter().enumerate() {
        if agents[..position].contains(&agent) {
            continue;
        }
        if agent == Agent::Paseo {
            materialize_paseo(baton)?;
            continue;
        }
        let adapter = baton_adapters::adapter(agent)?;
        let natives = match adapter.discover(None) {
            Ok(natives) => natives,
            Err(error) => {
                eprintln!("baton: {agent}: {error:#}");
                continue;
            }
        };
        let mut tally = import::Tally::default();
        import::import_sessions(baton, adapter.as_ref(), &natives, &mut tally);
        import::finish(baton, &tally)?;
        baton.refresh()?;
        let summary = baton.materialize(&*adapter, &natives, None)?;
        for warning in &summary.warnings {
            eprintln!("baton: {agent}: {warning}");
        }
        println!(
            "{agent}: {} exported, {} refreshed, {} removed, {} up to date, {} skipped",
            summary.exported,
            summary.refreshed,
            summary.removed,
            summary.up_to_date,
            summary.skipped
        );
    }
    Ok(())
}

fn materialize_paseo(baton: &mut Baton) -> Result<()> {
    let paseo = Paseo::new(Paseo::default_home()?);
    let available = paseo.available();
    if !available {
        eprintln!("baton: paseo daemon is not running; skipping registration and deletion");
    }
    let mut registrations = if available {
        export::discover_registrations(&paseo)?
    } else {
        HashMap::new()
    };
    let mut groups: HashMap<Agent, HashSet<String>> = HashMap::new();
    for (id, link) in baton.registry_sessions(Agent::Paseo)? {
        if let Some(provider) = link.provider
            && Paseo::supports(provider.agent)
        {
            groups.entry(provider.agent).or_default().insert(id);
        }
    }
    let mut summary = Materialized::default();
    for (provider, ids) in groups {
        let adapter = baton_adapters::adapter(provider)?;
        let natives = adapter.discover(None)?;
        for (native, outcome) in natives
            .iter()
            .zip(baton.import_all(adapter.as_ref(), &natives))
        {
            if let Err(error) = outcome {
                eprintln!("baton: {}:{}: {error:#}", adapter.agent(), native.id);
            }
        }
        baton.refresh()?;
        let result = baton.materialize(adapter.as_ref(), &natives, Some(&ids))?;
        summary += result;
        baton.refresh()?;
        if !available {
            continue;
        }
        let natives = adapter.discover(None)?;
        let mut natives_by_id = HashMap::new();
        for native in &natives {
            natives_by_id.entry(native.id.as_str()).or_insert(native);
        }
        for id in ids {
            let session = baton.store.load(&id)?;
            let latest = baton.index.latest_native(&id, provider)?;
            let native = latest
                .as_ref()
                .and_then(|id| natives_by_id.get(id.as_str()).copied());
            let own: Vec<_> = session
                .meta
                .links
                .iter()
                .filter(|link| link.host == baton.host() && link.agent == Agent::Paseo)
                .collect();
            if let Some(native) = native
                && !own.iter().any(|link| {
                    link.provider.as_ref().is_some_and(|reference| {
                        reference.agent == provider && reference.native_id == native.id
                    })
                })
            {
                let exported = Exported {
                    native: native.clone(),
                    cwd: materialization_cwd(&session)?,
                };
                if let Err(error) = export::register(
                    baton,
                    &paseo,
                    &mut registrations,
                    &id,
                    provider,
                    &exported,
                    session.meta.title.as_deref(),
                ) {
                    summary.warnings.push(format!("{id}: {error:#}"));
                }
            }
            for link in own {
                if link.direction != Direction::Export {
                    continue;
                }
                if link.provider.as_ref().is_some_and(|reference| {
                    native.is_some_and(|native| {
                        reference.agent == provider && reference.native_id == native.id
                    })
                }) {
                    continue;
                }
                match paseo.delete(&link.native_id) {
                    Ok(()) => {
                        registrations.retain(|_, agent| agent.id != link.native_id);
                        let mut removed = link.clone();
                        removed.removed = true;
                        baton.record_link(&id, removed, None)?;
                    }
                    Err(error) => summary
                        .warnings
                        .push(format!("{}: {error:#}", link.native_id)),
                }
            }
        }
    }
    for warning in &summary.warnings {
        eprintln!("baton: paseo: {warning}");
    }
    println!(
        "paseo: {} exported, {} refreshed, {} removed, {} up to date, {} skipped",
        summary.exported, summary.refreshed, summary.removed, summary.up_to_date, summary.skipped
    );
    Ok(())
}
