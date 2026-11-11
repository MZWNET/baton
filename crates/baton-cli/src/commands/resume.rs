use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_adapters::Paseo;
use baton_core::{Agent, Baton};

use super::export;
use crate::resolve;

// Takes the store by value so its lock is released before the agent starts
pub(crate) fn run(
    baton: Baton,
    query: &str,
    agent: Agent,
    provider: Option<Agent>,
    cwd: Option<&Path>,
) -> Result<()> {
    if agent == Agent::Rikkahub {
        bail!(
            "RikkaHub runs on Android: add the session to a backup with `baton export --to rikkahub --backup <file>` and restore that backup in the app"
        );
    }
    let (provider, adapter) = export::destination(agent, provider, None)?;
    // Only a running daemon opens the agent, so without one nothing is exported
    let paseo = match provider {
        Some(provider) => {
            let paseo = Paseo::new(Paseo::default_home()?);
            if !paseo.available() {
                bail!("paseo daemon is not running; cannot open a Paseo agent");
            }
            let registrations = paseo.discover()?;
            Some((provider, paseo, registrations))
        }
        None => None,
    };
    let session = resolve::session_id(&baton, query)?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;
    let command = match paseo {
        Some((provider, paseo, registrations)) => {
            let session = baton.store.load(&session)?;
            let id = export::register(
                &baton,
                &paseo,
                &registrations,
                &session.meta,
                provider,
                &exported,
            )?;
            paseo.open_command(&id)
        }
        None => adapter.resume_command(&exported.native, &exported.cwd),
    };
    drop(baton);

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
