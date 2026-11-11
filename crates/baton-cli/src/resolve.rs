use std::path::{self, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result, bail};
use baton_adapters::Paseo;
use baton_core::{Agent, Baton, ImportOutcome};

use crate::commands::import;

// Agents record their working directory as the OS reports it: with symbolic links resolved on Unix, and without the `\\?\` prefix canonical paths get on Windows
pub(crate) fn directory(value: &str) -> io::Result<PathBuf> {
    if cfg!(windows) {
        path::absolute(value)
    } else {
        fs::canonicalize(value)
    }
}

// A session id, a unique prefix of one, or `agent:` with a native id or a prefix matching one native session among those imported and never imported, importing a never imported one first
pub(crate) fn session_id(baton: &Baton, query: &str) -> Result<String> {
    let native_reference = query
        .split_once(':')
        .and_then(|(name, prefix)| Some((name.parse::<Agent>().ok()?, prefix)));
    let Some((agent, prefix)) = native_reference else {
        let session = baton.index.resolve(query)?;
        return session.with_context(|| format!("no session matches `{query}`"));
    };
    // As with session ids, an exact native id wins over the longer ones it prefixes, and needs no discovery
    if let Some(session) = baton.index.linked_session_any_host(Some(agent), prefix)? {
        return Ok(session);
    }
    let linked = baton.index.linked_session_by_prefix(agent, prefix)?;
    match agent {
        // Without a backup to read baton finds no RikkaHub session, so only imported ones resolve
        Agent::Rikkahub => linked.with_context(|| {
            format!(
                "no imported rikkahub session matches `{prefix}`; import the backup holding it with `baton import --agent rikkahub --path <backup>`"
            )
        }),
        Agent::Paseo => paseo_session(baton, prefix, linked),
        _ => native_session(baton, agent, prefix, linked),
    }
}

fn native_session(
    baton: &Baton,
    agent: Agent,
    prefix: &str,
    linked: Option<String>,
) -> Result<String> {
    let source = baton_adapters::source(agent)?;
    let mut unimported = Vec::new();
    for native in source.discover(None)? {
        if native.id.starts_with(prefix)
            && baton
                .index
                .linked_session_any_host(Some(agent), &native.id)?
                .is_none()
        {
            unimported.push(native);
        }
    }
    let native = match unique(agent, prefix, linked, unimported)? {
        Match::Linked(session) => return Ok(session),
        Match::Unimported(native) => native,
    };
    match baton.import(&*source, &native)? {
        ImportOutcome::Created { session }
        | ImportOutcome::Appended { session, .. }
        | ImportOutcome::Forked { session, .. } => Ok(session),
        ImportOutcome::Unchanged => bail!("{agent}:{} has no conversation", native.id),
    }
}

fn paseo_session(baton: &Baton, prefix: &str, linked: Option<String>) -> Result<String> {
    let paseo = Paseo::new(Paseo::default_home()?);
    let mut unimported = Vec::new();
    for registration in paseo.discover()? {
        if registration.id.starts_with(prefix)
            && baton
                .index
                .linked_session_any_host(Some(Agent::Paseo), &registration.id)?
                .is_none()
        {
            unimported.push(registration);
        }
    }
    let registration = match unique(Agent::Paseo, prefix, linked, unimported)? {
        Match::Linked(session) => return Ok(session),
        Match::Unimported(registration) => registration,
    };
    import::import_registration(baton, &paseo, &registration)?.with_context(|| {
        format!(
            "paseo:{} wraps no {} conversation baton can find",
            registration.id, registration.provider
        )
    })
}

enum Match<T> {
    Linked(String),
    Unimported(T),
}

// The linked session or the never imported native session a prefix matches, when it matches only one of them
fn unique<T>(
    agent: Agent,
    prefix: &str,
    linked: Option<String>,
    mut unimported: Vec<T>,
) -> Result<Match<T>> {
    match (linked, unimported.len()) {
        (Some(session), 0) => Ok(Match::Linked(session)),
        (None, 1) => Ok(Match::Unimported(unimported.remove(0))),
        (linked, count) => bail!(
            "{} {agent} sessions match `{prefix}`",
            count + usize::from(linked.is_some())
        ),
    }
}
