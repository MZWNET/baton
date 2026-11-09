use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_adapters::Paseo;
use baton_core::{Agent, Baton};

use super::export;

// Takes the store by value so its lock is released before the agent starts
pub(crate) fn run(
    mut baton: Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    cwd: Option<&Path>,
) -> Result<()> {
    let export::ExportedSession {
        session,
        provider,
        adapter,
        exported,
    } = export::export_session(&mut baton, query, agent, provider, cwd)?;
    if let Some(provider) = provider {
        let paseo = Paseo::new(Paseo::default_home()?);
        if !paseo.available() {
            bail!("paseo daemon is not running; cannot open a Paseo agent");
        }
        let title = baton.store.load(&session)?.meta.title;
        let mut registrations = export::discover_registrations(&paseo)?;
        let id = export::register(
            &baton,
            &paseo,
            &mut registrations,
            &session,
            provider,
            &exported,
            title.as_deref(),
        )?;
        drop(baton);

        return Err(hand_over(paseo.open_command(&id))).context("starting paseo");
    }
    drop(baton);
    let command = adapter.resume_command(&exported.native, &exported.cwd);

    Err(hand_over(command)).with_context(|| format!("starting {agent}"))
}

#[cfg(unix)]
fn hand_over(mut command: Command) -> io::Error {
    use std::os::unix::process::CommandExt;

    command.exec()
}

// Windows cannot replace a process, so baton waits for the agent and leaves Ctrl+C to it
#[cfg(windows)]
fn hand_over(mut command: Command) -> io::Error {
    use std::process;

    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    extern "system" fn ignore(_event: u32) -> i32 {
        1
    }

    // SAFETY: the handler is a plain function, valid for as long as the process runs
    unsafe { SetConsoleCtrlHandler(Some(ignore), 1) };
    match command.status() {
        Ok(status) => process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}
