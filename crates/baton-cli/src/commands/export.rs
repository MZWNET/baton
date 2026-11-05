use std::path::Path;

use anyhow::Result;
use baton_core::{Agent, Baton};

use crate::resolve;

pub(crate) fn run(baton: &mut Baton, query: &str, agent: Agent, cwd: Option<&Path>) -> Result<()> {
    let session = resolve::session_id(baton, query)?;
    let adapter = baton_adapters::adapter(agent)?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;
    println!("{}", exported.native.id);
    Ok(())
}
