use anyhow::Result;
use baton_core::{Baton, Direction};

use crate::{render, resolve};

pub(crate) fn run(baton: &mut Baton, query: &str) -> Result<()> {
    let id = resolve::session_id(baton, query)?;
    let session = baton.store.load(&id)?;
    let meta = &session.meta;
    println!("id       {}", meta.id);
    if let Some(title) = &meta.title {
        println!("title    {title}");
    }
    if let Some(cwd) = &meta.cwd {
        println!("cwd      {cwd}");
    }
    println!("created  {}", meta.created);
    println!("updated  {}", meta.updated);
    if let Some(parent) = &meta.parent {
        let at = parent.event.as_deref().unwrap_or("its start");
        println!("forked   from {} at {at}", parent.session);
    }
    for link in &meta.links {
        let direction = match link.direction {
            Direction::Import => "from",
            Direction::Export => "to",
        };
        println!(
            "link     {direction} {}:{}  {} events  {}",
            link.agent, link.native_id, link.exported_events, link.path
        );
    }
    for event in session.path() {
        println!();
        println!("{}", render::event(event));
    }
    Ok(())
}
