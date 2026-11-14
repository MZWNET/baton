use anyhow::{Result, bail};
use baton_core::{Adapter, Agent, Source};

use crate::{
    AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter, ClaudeAdapter,
    CodexAdapter, CopilotAdapter, CursorAdapter, CursorIdeAdapter, DevinAdapter, DshAdapter,
    GrokAdapter, HermesAdapter, KimiAdapter, MastracodeAdapter, MuseAdapter, OpencodeAdapter,
    OpenhandsAdapter, PiAdapter, QwenAdapter, RikkahubAdapter, VibeAdapter, ZcodeAdapter,
};

/// The source of `agent`'s sessions at the agent's default location, honoring the agent's own environment overrides such as `CODEX_HOME`.
///
/// Every agent but Paseo, which keeps no sessions of its own, has one; the source is boxed because the agent is usually only known at run time.
pub fn source(agent: Agent) -> Result<Box<dyn Source>> {
    let source: Box<dyn Source> = match agent {
        Agent::AntigravityDesktop => Box::new(AntigravityDesktopAdapter::new(
            AntigravityDesktopAdapter::default_root()?,
        )),
        Agent::AntigravityIde => Box::new(AntigravityIdeAdapter::new(
            AntigravityIdeAdapter::default_root()?,
            AntigravityIdeAdapter::default_user_data()?,
        )),
        Agent::CursorIde => Box::new(CursorIdeAdapter::new(CursorIdeAdapter::default_user_data()?)),
        writable => adapter(writable)?,
    };
    Ok(source)
}

/// The adapter that also writes `agent`'s sessions, at the agent's default location; fails for agents baton can only read, and for Paseo.
pub fn adapter(agent: Agent) -> Result<Box<dyn Adapter>> {
    let adapter: Box<dyn Adapter> = match agent {
        Agent::Pi | Agent::Omp => Box::new(PiAdapter::new(agent, PiAdapter::default_root(agent)?)),
        Agent::Claude => Box::new(ClaudeAdapter::new(ClaudeAdapter::default_root()?)),
        Agent::Codex => Box::new(CodexAdapter::new(CodexAdapter::default_root()?)),
        Agent::Opencode | Agent::Kilo => Box::new(OpencodeAdapter::new(
            agent,
            OpencodeAdapter::default_data(agent)?,
        )),
        Agent::Hermes => Box::new(HermesAdapter::new(HermesAdapter::default_home()?)),
        Agent::Mastracode => Box::new(MastracodeAdapter::new(MastracodeAdapter::default_db()?)),
        Agent::Devin => Box::new(DevinAdapter::new(DevinAdapter::default_db()?)),
        Agent::Copilot => Box::new(CopilotAdapter::new(CopilotAdapter::default_root()?)),
        Agent::Grok => Box::new(GrokAdapter::new(GrokAdapter::default_root()?)),
        Agent::Openhands => Box::new(OpenhandsAdapter::new(
            OpenhandsAdapter::default_conversations()?,
        )),
        Agent::Vibe => Box::new(VibeAdapter::new(VibeAdapter::default_sessions()?)),
        Agent::Qwen => Box::new(QwenAdapter::new(QwenAdapter::default_root()?)),
        Agent::Kimi => Box::new(KimiAdapter::new(KimiAdapter::default_root()?)),
        Agent::Muse => Box::new(MuseAdapter::new(MuseAdapter::default_root()?)),
        Agent::Antigravity => {
            Box::new(AntigravityAdapter::new(AntigravityAdapter::default_root()?))
        }
        Agent::Cursor => Box::new(CursorAdapter::new(CursorAdapter::default_root()?)),
        Agent::Dsh => Box::new(DshAdapter::new(DshAdapter::default_home()?)),
        // RikkaHub keeps its conversations on the phone, so only a backup handed to the adapter holds any
        Agent::Rikkahub => Box::new(RikkahubAdapter::new(None)),
        Agent::Zcode => Box::new(ZcodeAdapter::new(
            ZcodeAdapter::default_db()?,
            ZcodeAdapter::default_artifacts()?,
        )),
        Agent::AntigravityDesktop | Agent::AntigravityIde | Agent::CursorIde => {
            bail!("baton can only read {agent} sessions")
        }
        Agent::Paseo => bail!(
            "Paseo keeps no sessions of its own; baton reads and writes its agents through the sessions of their providers"
        ),
    };
    Ok(adapter)
}
