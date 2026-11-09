use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use baton_adapters::{Paseo, PaseoAgent};
use baton_core::{Adapter, Agent, Baton, Direction, Exported};

use crate::resolve;

fn paseo_provider(agent: Agent, provider: Option<Agent>) -> Result<Option<Agent>> {
    match (agent, provider) {
        (Agent::Paseo, Some(provider)) if Paseo::supports(provider) => Ok(Some(provider)),
        (Agent::Paseo, Some(_)) => {
            bail!("Paseo supports providers claude, codex, opencode, pi and omp")
        }
        (Agent::Paseo, None) => bail!("--provider is required with paseo"),
        (_, Some(_)) => bail!("--provider is only valid with paseo"),
        (_, None) => Ok(None),
    }
}

pub(crate) type Registrations = HashMap<(Agent, String), PaseoAgent>;

pub(crate) fn discover_registrations(paseo: &Paseo) -> Result<Registrations> {
    let mut registrations = HashMap::new();
    for agent in paseo.discover()? {
        registrations
            .entry((agent.provider, agent.handle.clone()))
            .or_insert(agent);
    }

    Ok(registrations)
}

pub(crate) fn register(
    baton: &Baton,
    paseo: &Paseo,
    registrations: &mut Registrations,
    session: &str,
    provider: Agent,
    exported: &Exported,
    title: Option<&str>,
) -> Result<String> {
    let handle = Paseo::handle(provider, &exported.native);
    let key = (provider, handle);
    let (agent, direction) = if let Some(agent) = registrations.get(&key) {
        (agent.clone(), Direction::Import)
    } else {
        let id = paseo.register(provider, &exported.native, &exported.cwd, title)?;
        let agent = paseo.record(&id)?;
        registrations.insert((agent.provider, agent.handle.clone()), agent.clone());
        (agent, Direction::Export)
    };
    baton.record_link(
        session,
        paseo.link(baton.host(), &agent, exported.native.id.clone(), direction),
        None,
    )?;

    Ok(agent.id)
}

pub(crate) struct ExportedSession {
    pub session: String,
    pub provider: Option<Agent>,
    pub adapter: Box<dyn Adapter>,
    pub exported: Exported,
}

pub(crate) fn export_session(
    baton: &mut Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    cwd: Option<&Path>,
) -> Result<ExportedSession> {
    let provider = paseo_provider(agent, provider)?;
    let session = resolve::session_id(baton, query)?;
    let adapter = baton_adapters::adapter(provider.unwrap_or(agent))?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;

    Ok(ExportedSession {
        session,
        provider,
        adapter,
        exported,
    })
}

pub(crate) fn run(
    baton: &mut Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    cwd: Option<&Path>,
) -> Result<()> {
    let ExportedSession {
        session,
        provider,
        exported,
        ..
    } = export_session(baton, query, agent, provider, cwd)?;
    if let Some(provider) = provider {
        let paseo = Paseo::new(Paseo::default_home()?);
        if paseo.available() {
            let title = baton.store.load(&session)?.meta.title;
            let mut registrations = discover_registrations(&paseo)?;
            println!(
                "{}",
                register(
                    baton,
                    &paseo,
                    &mut registrations,
                    &session,
                    provider,
                    &exported,
                    title.as_deref(),
                )?
            );
        } else {
            eprintln!("baton: paseo daemon is not running; skipping registration");
            println!("{}", exported.native.id);
        }
    } else {
        println!("{}", exported.native.id);
    }

    Ok(())
}
