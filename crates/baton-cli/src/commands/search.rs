use anyhow::Result;
use baton_core::Baton;

use crate::render;

const LIMIT: usize = 50;

pub(crate) fn run(baton: &Baton, terms: &[String]) -> Result<()> {
    for hit in baton.index.search(terms, LIMIT)? {
        let title = hit.title.as_deref().unwrap_or("-");
        println!(
            "{}  {}  {}  {title}",
            hit.session,
            render::minute(&hit.timestamp),
            hit.kind
        );
        println!("    {}", render::snippet(&hit.body, &terms[0]));
    }
    Ok(())
}
