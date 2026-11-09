use std::{fmt, str};

use serde::{Deserialize, Serialize};

/// A coding agent whose sessions baton reads, and writes unless it can only read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
    /// [Kilo Code](https://github.com/Kilo-Org/kilocode), a fork of OpenCode that keeps sessions in its SQLite database.
    Kilo,
    /// [Hermes Agent](https://github.com/NousResearch/hermes-agent), which keeps sessions in `~/.hermes/state.db`.
    Hermes,
    /// [MastraCode](https://github.com/mastra-ai/mastra/tree/main/mastracode), which keeps threads in its SQLite database `mastra.db`.
    Mastracode,
    /// [Devin CLI](https://docs.devin.ai/cli), which keeps sessions in `~/.local/share/devin/cli/sessions.db`.
    Devin,
    /// [GitHub Copilot CLI](https://github.com/github/copilot-cli), which keeps sessions under `~/.copilot/session-state`.
    Copilot,
    /// [Grok Build](https://github.com/xai-org/grok-build), which keeps sessions under `~/.grok/sessions`.
    Grok,
    /// [OpenHands CLI](https://github.com/OpenHands/OpenHands-CLI), which keeps conversations under `~/.openhands/conversations`.
    Openhands,
    /// [Mistral Vibe](https://github.com/mistralai/mistral-vibe), which keeps sessions under `~/.vibe/logs/session`.
    Vibe,
    /// [Qwen Code](https://github.com/QwenLM/qwen-code), which keeps chats under `~/.qwen/projects`.
    Qwen,
    /// [Kimi Code](https://github.com/MoonshotAI/kimi-code), which keeps sessions under `~/.kimi-code/sessions`.
    Kimi,
    /// [Muse Code](https://dev.meta.ai/docs/muse-code/), which keeps sessions under `~/.local/share/muse/sessions`.
    Muse,
    /// [Antigravity CLI](https://antigravity.google/docs/cli/), which keeps conversations under `~/.gemini/antigravity-cli`.
    Antigravity,
    /// The [Antigravity](https://antigravity.google/) desktop app, which keeps conversations under `~/.gemini/antigravity`; read only.
    AntigravityDesktop,
    /// The [Antigravity](https://antigravity.google/) IDE, which keeps conversation logs under `~/.gemini/antigravity-ide`; read only.
    AntigravityIde,
    /// [Cursor Agent](https://cursor.com/cli), which keeps chats under `~/.cursor/chats`.
    Cursor,
    /// [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness), which keeps sessions under `~/.dsh/sessions`.
    Dsh,
    /// The [Cursor](https://cursor.com/) editor, which keeps chats in its global state database; read only.
    CursorIde,
}

impl Agent {
    /// Every supported agent, in the order imports visit them.
    pub const ALL: [Agent; 22] = [
        Agent::Pi,
        Agent::Omp,
        Agent::Claude,
        Agent::Codex,
        Agent::Opencode,
        Agent::Kilo,
        Agent::Hermes,
        Agent::Mastracode,
        Agent::Devin,
        Agent::Copilot,
        Agent::Grok,
        Agent::Openhands,
        Agent::Vibe,
        Agent::Qwen,
        Agent::Kimi,
        Agent::Muse,
        Agent::Antigravity,
        Agent::AntigravityDesktop,
        Agent::AntigravityIde,
        Agent::Cursor,
        Agent::Dsh,
        Agent::CursorIde,
    ];

    /// The name used on the command line, in `agent:native_id` references and in stored events.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Pi => "pi",
            Agent::Omp => "omp",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Kilo => "kilo",
            Agent::Hermes => "hermes",
            Agent::Mastracode => "mastracode",
            Agent::Devin => "devin",
            Agent::Copilot => "copilot",
            Agent::Grok => "grok",
            Agent::Openhands => "openhands",
            Agent::Vibe => "vibe",
            Agent::Qwen => "qwen",
            Agent::Kimi => "kimi",
            Agent::Muse => "muse",
            Agent::Antigravity => "antigravity",
            Agent::AntigravityDesktop => "antigravity-desktop",
            Agent::AntigravityIde => "antigravity-ide",
            Agent::Cursor => "cursor",
            Agent::Dsh => "dsh",
            Agent::CursorIde => "cursor-ide",
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
