use std::path::PathBuf;

use anyhow::Result;
use baton_core::{Agent, Baton};
use clap::{Parser, Subcommand};

use crate::commands::{export, import, list, migrate, resume, search, show};
use crate::resolve;

#[derive(Parser)]
#[command(
    name = "baton",
    version,
    about = "Keep coding agent conversations in one append-only history and continue them in any agent"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import native sessions, reading only what changed since the last import.
    Import {
        /// Only this agent.
        #[arg(long)]
        agent: Option<Agent>,
        /// A session file, or an agent home to scan instead of the default one.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// List sessions, most recently updated first.
    List {
        /// Only sessions from this directory, the current one when given without a value.
        #[arg(long, num_args = 0..=1, default_missing_value = ".", value_parser = resolve::directory)]
        cwd: Option<PathBuf>,
        /// Only sessions linked to this agent.
        #[arg(long)]
        agent: Option<Agent>,
    },
    /// Search message, tool and summary text.
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
    },
    /// Print the active history of a session.
    Show {
        /// Session id, a unique prefix of it, or agent:native_id.
        id: String,
    },
    /// Write a session as a new native session and print its native id.
    Export {
        /// Session id, a unique prefix of it, or agent:native_id.
        id: String,
        #[arg(long)]
        to: Agent,
        /// Project directory of the native session, the session's own by default.
        #[arg(long, value_parser = resolve::directory)]
        cwd: Option<PathBuf>,
    },
    /// Export a session, then open it in the agent.
    Resume {
        /// Session id, a unique prefix of it, or agent:native_id.
        id: String,
        #[arg(long = "in", value_name = "AGENT")]
        agent: Agent,
        /// Project directory of the native session, the session's own by default.
        #[arg(long, value_parser = resolve::directory)]
        cwd: Option<PathBuf>,
    },
    /// Convert a data directory written by an older baton to the current layout.
    Migrate,
}

impl Cli {
    pub(crate) fn run(self) -> Result<()> {
        // Opening fails on the old layout that migrating converts
        if matches!(self.command, Command::Migrate) {
            return migrate::run();
        }
        let mut baton = Baton::open()?;
        match self.command {
            Command::Import { agent, path } => import::run(&mut baton, agent, path.as_deref()),
            Command::List { cwd, agent } => list::run(&baton, cwd.as_deref(), agent),
            Command::Search { terms } => search::run(&baton, &terms),
            Command::Show { id } => show::run(&mut baton, &id),
            Command::Export { id, to, cwd } => export::run(&mut baton, &id, to, cwd.as_deref()),
            Command::Resume { id, agent, cwd } => resume::run(baton, &id, agent, cwd.as_deref()),
            Command::Migrate => unreachable!("migrate runs without opening"),
        }
    }
}
