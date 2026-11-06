use std::path::Path;

use anyhow::Result;
use baton_core::{Agent, Baton};

use crate::render;

pub(crate) fn run(baton: &Baton, cwd: Option<&Path>, agent: Option<Agent>) -> Result<()> {
    let cwd = cwd.map(Path::to_string_lossy);
    for summary in baton.index.list(cwd.as_deref(), agent)? {
        let label = summary.title.or(summary.cwd).unwrap_or_else(|| "-".into());
        let branches = if summary.alternate_branches { " *" } else { "" };
        println!(
            "{}  {}  {:>5}  {:<14}  {label}{branches}",
            summary.id,
            render::minute(&summary.updated),
            summary.events,
            summary.agents.join(",")
        );
    }
    Ok(())
}
