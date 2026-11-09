use std::collections::{HashMap, HashSet};

use anyhow::Result;
use baton_adapters::Paseo;
use baton_core::{Adapter, Agent, Baton, Direction, Exported, Materialized, materialization_cwd};

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
        import::run(baton, Some(agent), None)?;
        baton.refresh()?;
        let natives = match adapter.discover(None) {
            Ok(natives) => natives,
            Err(error) => {
                eprintln!("baton: {agent}: {error:#}");
                continue;
            }
        };
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

fn import_provider(baton: &mut Baton, adapter: &dyn Adapter) -> Result<()> {
    for native in adapter.discover(None)? {
        if let Err(error) = baton.import(adapter, &native) {
            eprintln!("baton: {}:{}: {error:#}", adapter.agent(), native.id);
        }
    }
    Ok(())
}

fn materialize_paseo(baton: &mut Baton) -> Result<()> {
    let paseo = Paseo::new(Paseo::default_home()?);
    let available = paseo.available();
    if !available {
        eprintln!("baton: paseo daemon is not running; skipping registration and deletion");
    }
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
        import_provider(baton, adapter.as_ref())?;
        baton.refresh()?;
        let natives = adapter.discover(None)?;
        let result = baton.materialize(adapter.as_ref(), &natives, Some(&ids))?;
        summary.exported += result.exported;
        summary.refreshed += result.refreshed;
        summary.removed += result.removed;
        summary.up_to_date += result.up_to_date;
        summary.skipped += result.skipped;
        summary.warnings.extend(result.warnings);
        baton.refresh()?;
        if !available {
            continue;
        }
        let natives = adapter.discover(None)?;
        for id in ids {
            let session = baton.store.load(&id)?;
            let latest = baton.index.latest_native(&id, provider)?;
            let native = latest
                .as_ref()
                .and_then(|id| natives.iter().find(|native| &native.id == id));
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
                if let Err(error) = export::register(baton, &paseo, &id, provider, &exported) {
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
