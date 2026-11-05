use anyhow::Result;
use baton_core::{Adapter, Agent};

use crate::{ClaudeAdapter, CodexAdapter, OpencodeAdapter, PiAdapter};

/// The adapter for `agent` at the agent's default location, honoring the agent's own environment overrides such as `CODEX_HOME`.
///
/// The adapter is boxed because the agent is usually only known at run time.
pub fn adapter(agent: Agent) -> Result<Box<dyn Adapter>> {
    let adapter: Box<dyn Adapter> = match agent {
        Agent::Pi | Agent::Omp => Box::new(PiAdapter::new(agent, PiAdapter::default_root(agent)?)),
        Agent::Claude => Box::new(ClaudeAdapter::new(ClaudeAdapter::default_root()?)),
        Agent::Codex => Box::new(CodexAdapter::new(CodexAdapter::default_root()?)),
        Agent::Opencode => Box::new(OpencodeAdapter::new(OpencodeAdapter::default_data()?)),
    };
    Ok(adapter)
}
