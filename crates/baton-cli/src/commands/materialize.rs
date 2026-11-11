use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use baton_adapters::{Paseo, PaseoAgent};
use baton_core::{
    Adapter, Agent, Baton, Direction, Exported, Link, Materialized, NativeSession, Session,
    materialization_cwd,
};

use super::{export, import};

enum Target {
    Adapter(Box<dyn Adapter>),
    // Paseo keeps no sessions of its own and writes through the adapters of its providers
    Paseo(Paseo),
}

pub(crate) fn run(baton: &mut Baton, agents: &[Agent], backup: Option<&Path>) -> Result<()> {
    if backup.is_some() && !agents.contains(&Agent::Rikkahub) {
        bail!("--backup is only valid with rikkahub");
    }
    // Every target is set up before any is written, so a missing option fails the whole command
    let mut targets = Vec::new();
    for (position, &agent) in agents.iter().enumerate() {
        if agents[..position].contains(&agent) {
            continue;
        }
        targets.push(match agent {
            Agent::Paseo => Target::Paseo(Paseo::new(Paseo::default_home()?)),
            _ => Target::Adapter(export::adapter(
                agent,
                backup.filter(|_| agent == Agent::Rikkahub),
            )?),
        });
    }
    for target in targets {
        match target {
            Target::Adapter(adapter) => materialize_agent(baton, adapter.as_ref())?,
            Target::Paseo(paseo) => materialize_paseo(baton, &paseo)?,
        }
    }
    Ok(())
}

fn materialize_agent(baton: &mut Baton, adapter: &dyn Adapter) -> Result<()> {
    let agent = adapter.agent();
    let natives = match adapter.discover(None) {
        Ok(natives) => natives,
        // The backup was named on the command line, so one that cannot be read fails the command
        Err(error) if agent == Agent::Rikkahub => return Err(error),
        Err(error) => {
            eprintln!("baton: {agent}: {error:#}");
            return Ok(());
        }
    };
    let mut tally = import::Tally::default();
    import::import_sessions(baton, adapter, &natives, &mut tally);
    import::finish(baton, &tally)?;
    baton.refresh()?;
    // Whoever registered them, Paseo agents keep opening the provider sessions they point at
    let protected = if Paseo::supports(agent) {
        wrapped_natives(
            agent,
            &natives,
            &Paseo::new(Paseo::default_home()?).discover()?,
        )
    } else {
        HashSet::new()
    };
    report(
        agent,
        &baton.materialize(adapter, &natives, None, &protected)?,
    );
    Ok(())
}

// Restores every session that was a Paseo agent under each of its providers; without a running daemon only the provider sessions are written
fn materialize_paseo(baton: &mut Baton, paseo: &Paseo) -> Result<()> {
    let registrations = paseo.discover()?;
    let mut tally = import::Tally::default();
    import::import_registrations(baton, paseo, &registrations, &mut tally)?;
    let mut groups: HashMap<Agent, HashSet<String>> = HashMap::new();
    for (id, link) in baton.registry_sessions(Agent::Paseo)? {
        if let Some(provider) = link.provider
            && Paseo::supports(provider.agent)
        {
            groups.entry(provider.agent).or_default().insert(id);
        }
    }
    let mut providers = Vec::new();
    for (provider, ids) in groups {
        let adapter = baton_adapters::adapter(provider)?;
        match adapter.discover(None) {
            Ok(natives) => {
                import::import_sessions(baton, adapter.as_ref(), &natives, &mut tally);
                providers.push((adapter, natives, ids));
            }
            Err(error) => eprintln!("baton: {provider}: {error:#}"),
        }
    }
    import::finish(baton, &tally)?;
    baton.refresh()?;
    let available = paseo.available();
    if !available {
        eprintln!("baton: paseo daemon is not running; skipping registration and deletion");
    }
    let mut summary = Materialized::default();
    for (adapter, natives, ids) in providers {
        let protected = wrapped_natives(adapter.agent(), &natives, &registrations);
        summary += baton.materialize(adapter.as_ref(), &natives, Some(&ids), &protected)?;
        baton.refresh()?;
        if available {
            replace_registrations(baton, paseo, adapter.as_ref(), &ids, &mut summary)?;
        }
    }
    report(Agent::Paseo, &summary);
    Ok(())
}

// Registers the provider session each session was just materialized to, and only once Paseo holds that registration deletes the ones baton made for older provider sessions, whose sessions materializing then removes unless another Paseo agent opens them
fn replace_registrations(
    baton: &mut Baton,
    paseo: &Paseo,
    adapter: &dyn Adapter,
    ids: &HashSet<String>,
    summary: &mut Materialized,
) -> Result<()> {
    let provider = adapter.agent();
    let natives = adapter.discover(None)?;
    let natives_by_id: HashMap<&str, &NativeSession> = natives
        .iter()
        .map(|native| (native.id.as_str(), native))
        .collect();
    let registrations = paseo.discover()?;
    let mut unregistered = HashSet::new();
    for id in ids {
        let session = baton.store.load(id)?;
        let Some(current) = current_native(baton, &session, provider, &natives_by_id)? else {
            continue;
        };
        let exported = Exported {
            native: current.clone(),
            cwd: materialization_cwd(&session)?,
        };
        if let Err(error) = export::register(
            baton,
            paseo,
            &registrations,
            &session.meta,
            provider,
            &exported,
        ) {
            summary.warnings.push(format!("{id}: {error:#}"));
            continue;
        }
        for link in &session.meta.links {
            let unregistering = obsolete_registration(
                baton,
                adapter,
                &session,
                link,
                current,
                &natives_by_id,
                &registrations,
            )
            .and_then(|obsolete| {
                if obsolete {
                    paseo.delete(&link.native_id)?;
                }
                Ok(obsolete)
            });
            match unregistering {
                Ok(true) => {
                    let mut removed = link.clone();
                    removed.removed = true;
                    baton.record_link(id, removed, None)?;
                    unregistered.insert(id.clone());
                }
                Ok(false) => {}
                Err(error) => summary
                    .warnings
                    .push(format!("{}: {error:#}", link.native_id)),
            }
        }
    }
    if unregistered.is_empty() {
        return Ok(());
    }
    baton.refresh()?;
    let protected = wrapped_natives(provider, &natives, &paseo.discover()?);
    let pruned = baton.materialize(adapter, &natives, Some(&unregistered), &protected)?;
    summary.removed += pruned.removed;
    summary.warnings.extend(pruned.warnings);
    baton.refresh()
}

// The provider session materializing left at the active leaf: the newest one linked, when it was exported at that leaf or the leaf came from it
fn current_native<'a>(
    baton: &Baton,
    session: &Session,
    provider: Agent,
    natives: &HashMap<&str, &'a NativeSession>,
) -> Result<Option<&'a NativeSession>> {
    let Some(newest) = baton.index.latest_native(&session.meta.id, provider)? else {
        return Ok(None);
    };
    let path = session.path();
    let (Some(native), Some(leaf)) = (natives.get(newest.as_str()).copied(), path.last()) else {
        return Ok(None);
    };
    let holds_leaf = (leaf.origin.agent == provider && leaf.origin.native_id == newest)
        || session.meta.links.iter().any(|link| {
            link.host == baton.host()
                && link.agent == provider
                && link.native_id == newest
                && link.direction == Direction::Export
                && link.exported_leaf.as_deref() == Some(leaf.id.as_str())
        });

    Ok(holds_leaf.then_some(native))
}

// Whether a Paseo agent baton registered for an older provider session can go: Paseo still points it at that provider session, the index gives this session both, and nothing was added to the provider session since baton wrote it
fn obsolete_registration(
    baton: &Baton,
    adapter: &dyn Adapter,
    session: &Session,
    link: &Link,
    current: &NativeSession,
    natives: &HashMap<&str, &NativeSession>,
    registrations: &[PaseoAgent],
) -> Result<bool> {
    let provider = adapter.agent();
    let Some(wrapped) = &link.provider else {
        return Ok(false);
    };
    if link.host != baton.host()
        || link.agent != Agent::Paseo
        || link.direction != Direction::Export
        || wrapped.agent != provider
        || wrapped.native_id == current.id
    {
        return Ok(false);
    }
    let Some(native) = natives.get(wrapped.native_id.as_str()).copied() else {
        return Ok(false);
    };
    let handle = Paseo::handle(provider, native);
    let id = session.meta.id.as_str();
    if !registrations.iter().any(|registration| {
        registration.id == link.native_id
            && registration.provider == provider
            && registration.handle == handle
    }) || baton
        .index
        .linked_session(Agent::Paseo, &link.native_id)?
        .as_deref()
        != Some(id)
        || baton.index.linked_session(provider, &native.id)?.as_deref() != Some(id)
    {
        return Ok(false);
    }
    let native_link = session
        .meta
        .links
        .iter()
        .find(|candidate| {
            candidate.host == link.host
                && candidate.agent == provider
                && candidate.native_id == native.id
        })
        .expect("indexed native link exists");

    baton.untouched_export(adapter, native_link, native)
}

// The provider sessions Paseo agents point at
fn wrapped_natives(
    provider: Agent,
    natives: &[NativeSession],
    registrations: &[PaseoAgent],
) -> HashSet<String> {
    let handles: HashSet<&str> = registrations
        .iter()
        .filter(|registration| registration.provider == provider)
        .map(|registration| registration.handle.as_str())
        .collect();
    natives
        .iter()
        .filter(|native| handles.contains(Paseo::handle(provider, native).as_str()))
        .map(|native| native.id.clone())
        .collect()
}

fn report(agent: Agent, summary: &Materialized) {
    for warning in &summary.warnings {
        eprintln!("baton: {agent}: {warning}");
    }
    println!(
        "{agent}: {} exported, {} refreshed, {} removed, {} up to date, {} skipped",
        summary.exported, summary.refreshed, summary.removed, summary.up_to_date, summary.skipped
    );
}
