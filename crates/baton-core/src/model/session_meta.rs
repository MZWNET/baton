use serde::{Deserialize, Serialize};

use crate::Agent;

/// The contents of `meta.json`, the only file of a session that is rewritten instead of appended to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session id, a UUIDv7 that is also the session directory name.
    pub id: String,
    /// The session title.
    pub title: Option<String>,
    /// The project directory the session ran in.
    pub cwd: Option<String>,
    /// When the conversation started, as an RFC 3339 timestamp.
    pub created: String,
    /// The timestamp of the newest event.
    pub updated: String,
    /// Where the session forked off another one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkPoint>,
    /// The native sessions that are kept in sync with this one.
    #[serde(default)]
    pub links: Vec<Link>,
}

/// Where a session forked off another one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForkPoint {
    /// The session forked from.
    pub session: String,
    /// The last event both sessions share, or `None` when the fork copied a native session that no longer matched.
    pub event: Option<String>,
}

/// A native session that is kept in sync with a unified session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// The agent of the native session.
    pub agent: Agent,
    /// The agent's id for the native session.
    pub native_id: String,
    /// Where the native session was last found.
    pub path: String,
    /// How many replayed events the native session shares with the unified session.
    pub exported_events: usize,
    /// Whether the unified session was imported from the native session or exported to it.
    pub direction: Direction,
}

/// Which way a link was created, which decides how the native session is matched against the unified one when it is read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The unified session came from the native session, so their events share ids.
    Import,
    /// The native session was written from the unified session, so their events match only as replayed bodies.
    Export,
}

impl Link {
    pub(crate) fn is_for(&self, agent: Agent, native_id: &str) -> bool {
        self.agent == agent && self.native_id == native_id
    }
}
