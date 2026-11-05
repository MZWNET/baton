use std::path::{self, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result, bail};
use baton_core::{Agent, Baton, ImportOutcome, NativeSession};

// Agents record their working directory as the OS reports it: with symbolic links resolved on Unix, and without the `\\?\` prefix canonical paths get on Windows
pub(crate) fn directory(value: &str) -> io::Result<PathBuf> {
    if cfg!(windows) {
        path::absolute(value)
    } else {
        fs::canonicalize(value)
    }
}

// A session id, a unique prefix of one, or `agent:native_id`, importing that native session first when it was never imported
pub(crate) fn session_id(baton: &mut Baton, query: &str) -> Result<String> {
    let native_reference = query
        .split_once(':')
        .and_then(|(name, prefix)| Some((name.parse::<Agent>().ok()?, prefix)));
    let Some((agent, prefix)) = native_reference else {
        let session = baton.index.resolve(query)?;
        return session.with_context(|| format!("no session matches `{query}`"));
    };
    if let Some(session) = baton.index.linked_session_by_prefix(agent, prefix)? {
        return Ok(session);
    }
    let adapter = baton_adapters::adapter(agent)?;
    let mut matches: Vec<NativeSession> = Vec::new();
    for native in adapter.discover(None)? {
        if native.id.starts_with(prefix) {
            matches.push(native);
        }
    }
    let [native] = matches.as_slice() else {
        bail!("{} {agent} sessions match `{prefix}`", matches.len());
    };
    match baton.import(adapter.as_ref(), native)? {
        ImportOutcome::Created { session }
        | ImportOutcome::Appended { session, .. }
        | ImportOutcome::Forked { session, .. } => Ok(session),
        ImportOutcome::Unchanged => baton
            .index
            .linked_session(agent, &native.id)?
            .with_context(|| format!("{agent}:{} has no conversation", native.id)),
    }
}
