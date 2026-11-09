use std::path::Path;

use anyhow::{Result, bail};
use baton_adapters::Paseo;
use baton_core::{Agent, Baton, Direction};

use crate::resolve;

pub(crate) fn paseo_provider(agent: Agent, provider: Option<Agent>) -> Result<Option<Agent>> {
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

pub(crate) fn register(
    baton: &Baton,
    paseo: &Paseo,
    session: &str,
    provider: Agent,
    exported: &baton_core::Exported,
) -> Result<String> {
    let title = baton.store.load(session)?.meta.title;
    let agents = paseo.discover()?;
    let handle = Paseo::handle(provider, &exported.native);
    let existing = agents
        .into_iter()
        .find(|agent| agent.provider == provider && agent.handle == handle);
    let (agent, direction) = match existing {
        Some(agent) => (agent, Direction::Import),
        None => {
            let id = paseo.register(provider, &exported.native, &exported.cwd, title.as_deref())?;
            (paseo.registered(&id)?, Direction::Export)
        }
    };
    baton.record_link(
        session,
        paseo.link(baton.host(), &agent, exported.native.id.clone(), direction),
        None,
    )?;
    Ok(agent.id)
}

pub(crate) fn run(
    baton: &mut Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    cwd: Option<&Path>,
) -> Result<()> {
    let provider = paseo_provider(agent, provider)?;
    let session = resolve::session_id(baton, query)?;
    let adapter = baton_adapters::adapter(provider.unwrap_or(agent))?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;
    if let Some(provider) = provider {
        let paseo = Paseo::new(Paseo::default_home()?);
        if paseo.available() {
            println!(
                "{}",
                register(baton, &paseo, &session, provider, &exported)?
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
