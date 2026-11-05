//! The unified session format of baton, its append-only store, and the logic that keeps unified sessions in sync with the native sessions of coding agents.
//!
//! A unified session is a tree of [`Event`]s appended to `sessions/<id>/events.jsonl`, described by a [`SessionMeta`] in `meta.json`, with images kept as [`Blobs`]. Everything specific to one agent lives behind the [`Adapter`] trait, which the `baton-adapters` crate implements; [`Baton`] imports native sessions through adapters and exports unified sessions back to them.

mod adapter;
mod agent;
mod cache;
mod hash;
pub mod journal;
mod model;
pub mod replay;
mod storage;
mod sync;
pub mod time;

pub use adapter::{Adapter, Cursor, Export, Info, NativeRead, NativeSession, Progress, Stamp};
pub use agent::Agent;
pub use cache::{Index, SearchHit, SessionSummary};
pub use model::{
    Assistant, Body, Compaction, Direction, Draft, Event, ForkPoint, Image, Link, Origin, Session,
    SessionMeta, Thinking, ToolCall, ToolResult, User,
};
pub use storage::{Blobs, Store};
pub use sync::{Baton, Exported, ImportOutcome};
