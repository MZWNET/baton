use std::io::{self, BufWriter, Write};

use anyhow::Result;
use baton_core::{Baton, Direction};

use crate::{render, resolve};

pub(crate) fn run(baton: &mut Baton, query: &str) -> Result<()> {
    let id = resolve::session_id(baton, query)?;
    let session = baton.store.load(&id)?;
    let meta = &session.meta;
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    writeln!(output, "id       {}", meta.id)?;
    if let Some(title) = &meta.title {
        writeln!(output, "title    {title}")?;
    }
    if let Some(cwd) = &meta.cwd {
        writeln!(output, "cwd      {cwd}")?;
    }
    if let Some(project) = &meta.project {
        writeln!(
            output,
            "project  {}  {}  {}",
            project.remote.as_deref().unwrap_or("-"),
            project.root.as_deref().unwrap_or("-"),
            project.path
        )?;
    }
    writeln!(output, "created  {}", meta.created)?;
    writeln!(output, "updated  {}", meta.updated)?;
    if let Some(parent) = &meta.parent {
        let at = parent.event.as_deref().unwrap_or("its start");
        writeln!(output, "forked   from {} at {at}", parent.session)?;
    }
    for link in &meta.links {
        let direction = match link.direction {
            Direction::Import => "from",
            Direction::Export => "to",
        };
        writeln!(
            output,
            "link     {direction} {}:{}  @{}  {} events  {}",
            link.agent, link.native_id, link.host, link.exported_events, link.path
        )?;
    }
    for event in session.path() {
        writeln!(output)?;
        render::event(&mut output, event)?;
        writeln!(output)?;
    }
    for branch in session.branches() {
        let leaf = branch.last().unwrap();
        let text = branch
            .iter()
            .rev()
            .map(|event| event.body.text())
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default();
        let prefix: String = leaf.id.chars().take(8).collect();
        writeln!(
            output,
            "branch   {prefix}  {}  {}",
            leaf.timestamp,
            render::snippet(&text)
        )?;
    }
    output.flush()?;

    Ok(())
}
