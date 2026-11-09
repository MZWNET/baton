//! Adapters between [`baton_core`] and the native sessions of every agent in [`baton_core::Agent`].
//!
//! Each adapter implements [`baton_core::Source`], which finds sessions in the agent's own store and parses them into events, and, unless baton can only read the agent, [`baton_core::Adapter`], which writes events back as a new session the agent can resume. Use [`source`] or [`adapter`] to get them for an agent at its default location.

mod antigravity;
mod claude;
mod codex;
mod copilot;
mod cursor;
mod devin;
mod dsh;
mod grok;
mod hermes;
mod kimi;
mod mastracode;
mod muse;
mod opencode;
mod openhands;
mod pi;
mod qwen;
mod registry;
mod support;
mod vibe;

pub use antigravity::{AntigravityAdapter, AntigravityDesktopAdapter, AntigravityIdeAdapter};
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use copilot::CopilotAdapter;
pub use cursor::{CursorAdapter, CursorIdeAdapter};
pub use devin::DevinAdapter;
pub use dsh::DshAdapter;
pub use grok::GrokAdapter;
pub use hermes::HermesAdapter;
pub use kimi::KimiAdapter;
pub use mastracode::MastracodeAdapter;
pub use muse::MuseAdapter;
pub use opencode::OpencodeAdapter;
pub use openhands::OpenhandsAdapter;
pub use pi::PiAdapter;
pub use qwen::QwenAdapter;
pub use registry::{adapter, source};
pub use vibe::VibeAdapter;
