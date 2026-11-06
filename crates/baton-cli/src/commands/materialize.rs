use anyhow::Result;
use baton_core::{Agent, Baton};

use super::import;

pub(crate) fn run(baton: &mut Baton, agents: &[Agent]) -> Result<()> {
    for (position, &agent) in agents.iter().enumerate() {
        if agents[..position].contains(&agent) {
            continue;
        }
        let adapter = baton_adapters::adapter(agent)?;
        import::run(baton, Some(agent), None)?;
        baton.refresh()?;
        let natives = match adapter.discover(None) {
            Ok(natives) => natives,
            Err(error) => {
                eprintln!("baton: {agent}: {error:#}");
                continue;
            }
        };
        let summary = baton.materialize(&*adapter, &natives)?;
        for warning in &summary.warnings {
            eprintln!("baton: {agent}: {warning}");
        }
        println!(
            "{agent}: {} exported, {} refreshed, {} removed, {} up to date, {} skipped",
            summary.exported,
            summary.refreshed,
            summary.removed,
            summary.up_to_date,
            summary.skipped
        );
    }
    Ok(())
}
