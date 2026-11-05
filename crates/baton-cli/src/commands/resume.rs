use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use baton_core::{Agent, Baton};

use crate::resolve;

// Takes the store by value so its lock is released before the agent starts
pub(crate) fn run(mut baton: Baton, query: &str, agent: Agent, cwd: Option<&Path>) -> Result<()> {
    let session = resolve::session_id(&mut baton, query)?;
    let adapter = baton_adapters::adapter(agent)?;
    let exported = baton.export(&session, adapter.as_ref(), cwd)?;
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
