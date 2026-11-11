use std::path::PathBuf;

use anyhow::Result;
use baton_core::{Agent, Baton};
use clap::{Parser, Subcommand};

use crate::commands::{export, import, list, materialize, migrate, resume, show};
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
        /// A session file, a RikkaHub backup, or an agent home to scan instead of the default one
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
        /// Provider to register in Paseo.
        #[arg(long)]
        provider: Option<Agent>,
        /// RikkaHub backup to add the session to, created when it does not exist
        #[arg(long)]
        backup: Option<PathBuf>,
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
        /// Provider to register in Paseo.
        #[arg(long)]
        provider: Option<Agent>,
        /// Project directory of the native session, the session's own by default.
        #[arg(long, value_parser = resolve::directory)]
        cwd: Option<PathBuf>,
    },
    /// Copy all sessions into the selected agents' native stores.
    Materialize {
        /// Agent to materialize into; may be repeated.
        #[arg(long = "in", value_name = "AGENT", required = true)]
        agents: Vec<Agent>,
        /// RikkaHub backup to add the sessions to, created when it does not exist
        #[arg(long)]
        backup: Option<PathBuf>,
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
            Command::Show { id } => show::run(&mut baton, &id),
            Command::Export {
                id,
                to,
                provider,
                backup,
                cwd,
            } => export::run(
                &mut baton,
                &id,
                to,
                provider,
                backup.as_deref(),
                cwd.as_deref(),
            ),
            Command::Resume {
                id,
                agent,
                provider,
                cwd,
            } => resume::run(baton, &id, agent, provider, cwd.as_deref()),
            Command::Materialize { agents, backup } => {
                materialize::run(&mut baton, &agents, backup.as_deref())
            }
            Command::Migrate => unreachable!("migrate runs without opening"),
        }
    }
}
