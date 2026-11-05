//! Adapters between [`baton_core`] and the native sessions of pi, Oh My Pi, Claude Code, Codex and OpenCode.
//!
//! Each adapter implements [`baton_core::Adapter`]: it finds sessions in the agent's own store, parses them into events, and writes events back as a new session the agent can resume. Use [`adapter`] to get the adapter for an agent at its default location.

mod claude;
mod codex;
mod opencode;
mod pi;
mod registry;
mod support;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use opencode::OpencodeAdapter;
pub use pi::PiAdapter;
pub use registry::adapter;
