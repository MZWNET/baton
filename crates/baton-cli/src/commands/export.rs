use std::path::Path;

use anyhow::{Result, bail};
use baton_adapters::{Paseo, PaseoAgent, RikkahubAdapter};
use baton_core::{Adapter, Agent, Baton, Direction, Exported, SessionMeta};

use super::import;
use crate::resolve;

// RikkaHub keeps its conversations on the phone, so baton writes them into a backup exported from the app
pub(crate) fn adapter(agent: Agent, backup: Option<&Path>) -> Result<Box<dyn Adapter>> {
    match (agent, backup) {
        (Agent::Rikkahub, Some(backup)) => {
            Ok(Box::new(RikkahubAdapter::new(Some(backup.to_path_buf()))))
        }
        (Agent::Rikkahub, None) => bail!("--backup is required with rikkahub"),
        (_, Some(_)) => bail!("--backup is only valid with rikkahub"),
        (_, None) => baton_adapters::adapter(agent),
    }
}

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

// The Paseo provider and the adapter writing the export, checked before resolving the session, which may import it
pub(crate) fn destination(
    agent: Agent,
    provider: Option<Agent>,
    backup: Option<&Path>,
) -> Result<(Option<Agent>, Box<dyn Adapter>)> {
    let provider = paseo_provider(agent, provider)?;
    let adapter = adapter(provider.unwrap_or(agent), backup)?;

    Ok((provider, adapter))
}

// Links the session to the Paseo agent of its exported provider session, registering one when Paseo has none, and returns the agent id
pub(crate) fn register(
    baton: &Baton,
    paseo: &Paseo,
    registrations: &[PaseoAgent],
    session: &SessionMeta,
    provider: Agent,
    exported: &Exported,
) -> Result<String> {
    let handle = Paseo::handle(provider, &exported.native);
    let created;
    let (registration, direction) = match registrations
        .iter()
        .find(|registration| registration.provider == provider && registration.handle == handle)
    {
        Some(registration) => (registration, Direction::Import),
        None => {
            let id = paseo.register(
                provider,
                &exported.native,
                &exported.cwd,
                session.title.as_deref(),
            )?;
            created = paseo.record(&id)?;
            (&created, Direction::Export)
        }
    };
    import::link_registration(
        baton,
        paseo,
        session,
        registration,
        exported.native.id.clone(),
        direction,
        None,
    )?;

    Ok(registration.id.clone())
}

pub(crate) fn run(
    baton: &mut Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    backup: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<()> {
    let (provider, adapter) = destination(agent, provider, backup)?;
    let session = resolve::session_id(baton, query)?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;
    let Some(provider) = provider else {
        println!("{}", exported.native.id);
        return Ok(());
    };
    let paseo = Paseo::new(Paseo::default_home()?);
    if !paseo.available() {
        eprintln!("baton: paseo daemon is not running; skipping registration");
        println!("{}", exported.native.id);
        return Ok(());
    }
    let registrations = paseo.discover()?;
    let session = baton.store.load(&session)?;
    println!(
        "{}",
        register(
            baton,
            &paseo,
            &registrations,
            &session.meta,
            provider,
            &exported,
        )?
    );

    Ok(())
}
