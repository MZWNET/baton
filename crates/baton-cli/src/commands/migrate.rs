use anyhow::Result;
use baton_core::Baton;

pub(crate) fn run() -> Result<()> {
    let migrated = Baton::migrate()?;
    println!("{migrated} sessions migrated");
    Ok(())
}
