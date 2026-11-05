use std::{fmt, str};

use serde::{Deserialize, Serialize};

/// A coding agent whose sessions baton reads and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    /// [pi](https://github.com/earendil-works/pi), which keeps sessions under `~/.pi/agent`.
    Pi,
    /// [Oh My Pi](https://github.com/can1357/oh-my-pi), a fork of pi that keeps sessions under `~/.omp/agent`.
    Omp,
    /// [Claude Code](https://github.com/anthropics/claude-code), which keeps sessions under `~/.claude/projects`.
    Claude,
    /// [Codex](https://github.com/openai/codex), which keeps rollouts under `~/.codex/sessions`.
    Codex,
    /// [OpenCode](https://github.com/anomalyco/opencode), which keeps sessions in its SQLite database.
    Opencode,
}

impl Agent {
    /// Every supported agent, in the order imports visit them.
    pub const ALL: [Agent; 5] = [
        Agent::Pi,
        Agent::Omp,
        Agent::Claude,
        Agent::Codex,
        Agent::Opencode,
    ];

    /// The name used on the command line, in `agent:native_id` references and in stored events.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Pi => "pi",
            Agent::Omp => "omp",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl str::FromStr for Agent {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Agent::ALL
            .into_iter()
            .find(|agent| agent.name() == value)
            .ok_or_else(|| {
                let names = Agent::ALL.map(Agent::name).join(", ");
                format!("unknown agent `{value}`, expected one of: {names}")
            })
    }
}
