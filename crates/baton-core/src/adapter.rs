use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use crate::{Agent, Blobs, Draft, Event};

/// The size and modification time of a native session, compared between imports to skip sessions that did not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Size in bytes, or 0 for sessions that are not files.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch, or the agent's own update time for sessions that are not files.
    pub mtime: i64,
}

impl Stamp {
    /// Stamps the file at `path`, following symbolic links.
    pub fn of(path: &Path) -> Result<Stamp> {
        let metadata = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        let mtime = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as i64);
        Ok(Stamp {
            size: metadata.len(),
            mtime,
        })
    }
}

/// A session found in the store of an agent.
#[derive(Clone, Debug)]
pub struct NativeSession {
    /// The agent's own id for the session.
    pub id: String,
    /// The session file, or the database holding the session for agents without session files.
    pub path: PathBuf,
    /// The version of the session when it was found.
    pub stamp: Stamp,
    /// The title, set by agents that keep titles outside the session file so that renames show up without content changes.
    pub title: Option<String>,
}

/// How far an import got through a native session file.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// Byte offset just past the last complete line that was read.
    pub offset: u64,
    /// SHA-256 of the bytes just before `offset`; a different hash means the file was rewritten and must be read in full.
    pub tail_hash: String,
    /// Native id of the entry the read ended on; new entries have to continue it to be read incrementally.
    pub leaf: Option<String>,
}

/// What baton remembers about the last import of a native session.
#[derive(Clone, Debug)]
pub struct Cursor {
    /// The file `progress` refers to.
    pub path: PathBuf,
    /// The stamp of the session at its last import.
    pub stamp: Stamp,
    /// How far the last import read.
    pub progress: Progress,
    /// The unified event the native session ended on.
    pub event: Option<String>,
    /// The unified session the native session is linked to.
    pub session: String,
}

/// Facts about a native session as a whole; each is `None` when the read did not come across it.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// The project directory the session ran in.
    pub cwd: Option<String>,
    /// The session title.
    pub title: Option<String>,
    /// When the session started, as an RFC 3339 timestamp.
    pub created: Option<String>,
}

/// The outcome of reading a native session.
pub struct NativeRead {
    /// Session level facts the read came across.
    pub info: Info,
    /// The conversation as events without ids: the whole active branch, or only what follows the cursor when `continued` is set.
    pub drafts: Vec<Draft>,
    /// Whether `drafts` continue the event recorded in the cursor instead of starting from the root.
    pub continued: bool,
    /// How far this read got, for the next incremental read.
    pub progress: Progress,
}

/// Everything an adapter needs to write a unified session as a new native session.
pub struct Export<'a> {
    /// The session title.
    pub title: Option<&'a str>,
    /// The project directory the native session belongs to.
    pub cwd: &'a Path,
    /// When the session started, as an RFC 3339 timestamp.
    pub created: &'a str,
    /// The events to write, as selected by [`replay::events`](crate::replay::events).
    pub events: &'a [&'a Event],
}

/// Reads and writes the sessions of one agent.
///
/// Reading has to preserve whatever writing produced: reading a written session must give back the written events, as compared by [`replay::digest`](crate::replay::digest), so that a session continued in an agent can flow back into the unified session it was exported from.
pub trait Adapter {
    /// The agent this adapter handles.
    fn agent(&self) -> Agent;

    /// Lists the sessions under the agent's default location, or under `path` when it is given: either an agent home directory or a single session file.
    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>>;

    /// Reads a session, continuing after `cursor` when the session only grew since; images are stored in `blobs`.
    fn read(
        &self,
        session: &NativeSession,
        cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead>;

    /// Writes the events as a new native session without overwriting anything; images are read from `blobs`.
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession>;

    /// The command that opens a session in the agent, running in `cwd`.
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command;
}
