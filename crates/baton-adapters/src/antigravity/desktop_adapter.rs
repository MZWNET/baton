use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use baton_core::{Agent, Blobs, Cursor, Info, NativeRead, NativeSession, Progress, Source};
use rusqlite::OptionalExtension;

use crate::antigravity::parser;
use crate::antigravity::summary::{self, Summary};
use crate::support::protobuf::Message;
use crate::support::{files, sqlite};

/// Conversations of the [Antigravity](https://antigravity.google/) desktop app: the CLI's conversation databases under `~/.gemini/antigravity`, with titles and workspaces in the app's summary cache.
///
/// baton reads them but cannot write them: no command opens a given conversation in the app. Archived conversations, which the app empties, and subagent conversations stay out.
pub struct AntigravityDesktopAdapter {
    root: PathBuf,
}

impl AntigravityDesktopAdapter {
    /// An adapter for the desktop app's data directory `root`.
    pub fn new(root: PathBuf) -> AntigravityDesktopAdapter {
        AntigravityDesktopAdapter { root }
    }

    /// The data directory, `~/.gemini/antigravity`.
    pub fn default_root() -> Result<PathBuf> {
        Ok(files::home()?.join(".gemini").join("antigravity"))
    }

    // The app rewrites its cache whole, so a corrupt or missing one only costs the titles
    fn summaries(root: &Path) -> HashMap<String, Summary> {
        let bytes = fs::read(root.join("agyhub_summaries_proto.pb")).unwrap_or_default();
        summary::entries(&bytes)
            .into_iter()
            .filter_map(|(id, bytes)| Some((id, summary::summary(&bytes)?)))
            .collect()
    }
}

// The app empties an archived conversation and keeps its trajectory, while a subagent's trajectory names its parent
fn listed(root: &Path, db: &Path, id: &str) -> Result<bool> {
    if archived(
        &fs::read_to_string(root.join("annotations").join(format!("{id}.pbtxt")))
            .unwrap_or_default(),
    ) {
        return Ok(false);
    }
    let connection = sqlite::open_read_only(db)?;
    let cascade: Option<String> = connection
        .query_row(
            "SELECT cascade_id FROM trajectory_meta WHERE cascade_id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let metadata = Message::parse(blob.as_deref().unwrap_or_default())?;
    let child = [5, 8].iter().any(|&field| metadata.bytes(field).is_some())
        || metadata.varint(17).is_some_and(|depth| depth > 0);
    Ok(cascade.is_some() && !child)
}

// Whether the text-format annotations mark a conversation archived at their top level
fn archived(annotations: &str) -> bool {
    let mut depth = 0;
    let mut quoted = false;
    let mut top = String::new();
    let mut previous = ' ';
    for char in annotations.chars() {
        match char {
            '"' if previous != '\\' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => depth -= 1,
            _ if !quoted && depth == 0 => top.push(char),
            _ => {}
        }
        previous = char;
    }
    let tokens: Vec<&str> = top
        .split([' ', ':', '\n', '\t'])
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .windows(2)
        .any(|pair| pair[0] == "archived" && matches!(pair[1], "true" | "True" | "t" | "1"))
}

impl Source for AntigravityDesktopAdapter {
    fn agent(&self) -> Agent {
        Agent::AntigravityDesktop
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let (root, dbs) = parser::databases(&self.root, path)?;
        let summaries = AntigravityDesktopAdapter::summaries(root);
        let mut sessions = Vec::new();
        for (db, id) in dbs {
            if !listed(root, &db, &id)? {
                continue;
            }
            sessions.push(NativeSession {
                stamp: sqlite::stamp(&db)?,
                title: summaries.get(&id).and_then(|summary| summary.title.clone()),
                path: db,
                id,
            });
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let connection = sqlite::open_read_only(&session.path)?;
        let conversation = parser::read(&connection, blobs)?;
        let summary = session
            .path
            .parent()
            .and_then(Path::parent)
            .and_then(|root| fs::read(root.join("agyhub_summaries_proto.pb")).ok())
            .and_then(|bytes| summary::entry(&bytes, &session.id).and_then(summary::summary));
        let summary = summary.as_ref();
        Ok(NativeRead {
            info: Info {
                cwd: summary
                    .and_then(|summary| summary.cwd.clone())
                    .or(conversation.cwd),
                title: summary.and_then(|summary| summary.title.clone()),
                created: summary
                    .and_then(|summary| summary.created.clone())
                    .or(conversation.created),
            },
            drafts: conversation.drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_top_level_archived_flags_count() {
        assert!(archived(
            "archived:true  archival_status_timestamp:{seconds:1782708366  nanos:907334000}"
        ));
        assert!(!archived(r#"title:"archived:true""#));
        assert!(!archived("nested:{archived:true}"));
        assert!(!archived("archived:false"));
    }
}
