use anyhow::Result;
use baton_core::Baton;

pub(crate) fn run() -> Result<()> {
    let migrated = Baton::migrate()?;
    if migrated.converted > 0 {
        println!(
            "{} sessions converted from the old layout",
            migrated.converted
        );
    }
    if migrated.rewritten > 0 {
        println!("{} sessions had call ids rewritten", migrated.rewritten);
    }
    if migrated.corrected > 0 {
        println!(
            "{} sessions had project remotes corrected",
            migrated.corrected
        );
    }
    if migrated.converted == 0 && migrated.rewritten == 0 && migrated.corrected == 0 {
        println!("Nothing needed migrating");
    }

    Ok(())
}
