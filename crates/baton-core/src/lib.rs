//! The unified session format of baton, its write-once store, and the logic that keeps unified sessions in sync with the native sessions of coding agents.
//!
//! A unified session is a tree of [`Event`]s in compressed segments, described by immutable records and loaded as [`SessionMeta`], with images kept as [`Blobs`]. Everything specific to one agent lives behind the [`Source`] and [`Adapter`] traits, which the `baton-adapters` crate implements; [`Baton`] imports native sessions through sources and exports unified sessions back through adapters.

mod adapter;
mod agent;
mod cache;
mod hash;
pub mod journal;
mod model;
pub mod project;
pub mod replay;
mod storage;
mod sync;
pub mod time;

pub use adapter::{
    Adapter, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source, Stamp,
};
pub use agent::Agent;
pub use cache::{Index, SearchHit, SessionSummary};
pub use model::{
    Assistant, Body, Compaction, Direction, Draft, Event, ForkPoint, Image, Link, Origin, Session,
    SessionMeta, Thinking, ToolCall, ToolResult, User,
};
pub use project::Project;
pub use storage::{Blobs, Store};
pub use sync::{Baton, Exported, ImportOutcome, Materialized};
