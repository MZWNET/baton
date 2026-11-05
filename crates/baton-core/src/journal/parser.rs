use anyhow::Result;
use serde_json::Value;

use crate::{Draft, Info};

/// Turns the lines of one native session file into tree nodes.
pub trait Parser {
    /// Interprets the JSON value of the line starting at byte `offset`.
    fn line(&mut self, offset: u64, value: Value) -> Result<Line>;

    /// The session level facts gathered from the lines, once all of them have been read.
    fn finish(self) -> Info;
}

/// How a line takes part in the conversation tree.
pub enum Line {
    /// The line is an entry of the tree.
    Node(Node),
    /// The line is not part of the tree, such as a session header or a title record.
    Skip,
    /// The line cannot be handled incrementally, so the whole file has to be read again.
    Restart,
}

/// An entry of the conversation tree.
pub struct Node {
    /// The native id of the entry.
    pub id: String,
    /// The native id of the parent entry, or `None` for a root.
    pub parent: Option<String>,
    /// The events the entry contributes, often none.
    pub drafts: Vec<Draft>,
    /// A native entry on the same branch whose events these must come before, like the first kept entry of a pi compaction.
    pub anchor: Option<String>,
}
