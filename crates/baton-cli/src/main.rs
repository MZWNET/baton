//! The `baton` command: keeps the conversations of coding agents in one append-only history and continues them in any agent.

mod cli;
mod commands;
mod render;
mod resolve;

use std::process::ExitCode;

use cli::Cli;

fn main() -> ExitCode {
    use clap::Parser;

    match Cli::parse().run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("baton: {error:#}");
            ExitCode::FAILURE
        }
    }
}
