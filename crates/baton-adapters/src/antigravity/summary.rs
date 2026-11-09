use std::collections::HashMap;

use crate::antigravity::parser::timestamp;
use crate::support::protobuf::Message;
use crate::support::uri;

// What Antigravity's summary of a trajectory says about its session
pub(super) struct Summary {
    pub(super) title: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) created: Option<String>,
}

// A `CascadeTrajectorySummary`: the title, the workspaces and the creation time
pub(super) fn summary(bytes: &[u8]) -> Option<Summary> {
    let message = Message::parse(bytes).ok()?;
    let workspace = message
        .repeated(9)
        .find_map(|workspace| Message::parse(workspace).ok()?.string(1).map(String::from));
    let metadata = message.message(17);
    let link = workspace.or_else(|| {
        metadata?
            .repeated(7)
            .next()
            .and_then(|uri| std::str::from_utf8(uri).ok().map(String::from))
    });
    Some(Summary {
        title: message
            .string(1)
            .filter(|title| !title.is_empty())
            .map(String::from),
        cwd: link.and_then(|link| uri::path(&link)),
        created: message.message(7).and_then(|time| timestamp(&time)),
    })
}

// Map entries of conversation ids and summaries, where a later entry for an id replaces an earlier one
pub(super) fn entries(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut entries = HashMap::new();
    let Ok(map) = Message::parse(bytes) else {
        return entries;
    };
    for entry in map.repeated(1) {
        if let Ok(entry) = Message::parse(entry)
            && let (Some(id), Some(value)) = (entry.string(1), entry.bytes(2))
        {
            entries.insert(id.to_owned(), value.to_vec());
        }
    }
    entries
}

pub(super) fn entry<'a>(bytes: &'a [u8], id: &str) -> Option<&'a [u8]> {
    let map = Message::parse(bytes).ok()?;
    let mut found = None;
    for entry in map.repeated(1) {
        if let Ok(entry) = Message::parse(entry)
            && entry.string(1) == Some(id)
            && let Some(value) = entry.bytes(2)
        {
            found = Some(value);
        }
    }

    found
}
