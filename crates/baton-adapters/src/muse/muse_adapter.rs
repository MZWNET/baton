use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal, time,
};
use chrono::Local;
use uuid::Uuid;

use crate::muse::log_parser::LogParser;
use crate::muse::log_writer::LogWriter;
use crate::support::{files, identifiers, program};

/// Sessions of [Muse Code](https://dev.meta.ai/docs/muse-code/): `sessions/YYYY/MM/DD/<session uuid>/session.jsonl` under its data directory.
///
/// Muse keeps neither image bytes nor summaries in its logs, so baton writes them as marked text that Muse replays to the model and baton reads back; subagent logs stay out.
pub struct MuseAdapter {
    root: PathBuf,
}

impl MuseAdapter {
    /// An adapter for the Muse data directory `root`.
    pub fn new(root: PathBuf) -> MuseAdapter {
        MuseAdapter { root }
    }

    /// The data directory: `$XDG_DATA_HOME/muse`, or else `~/.local/share/muse`, on every platform.
    pub fn default_root() -> Result<PathBuf> {
        Ok(files::xdg_data_home()?.join("muse"))
    }

    // A session directory is named after the session's UUID; subagent logs lie deeper
    fn probe(log: &Path) -> Result<Option<NativeSession>> {
        let Some(id) = log
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| Uuid::parse_str(name).is_ok())
        else {
            return Ok(None);
        };
        if !log.is_file() {
            return Ok(None);
        }
        Ok(Some(NativeSession {
            id: id.to_owned(),
            path: log.to_path_buf(),
            stamp: Stamp::of(log)?,
            title: None,
        }))
    }
}

impl Source for MuseAdapter {
    fn agent(&self) -> Agent {
        Agent::Muse
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(MuseAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for year in files::read_dir(&root.join("sessions"))? {
            for month in files::read_dir(&year)? {
                for day in files::read_dir(&month)? {
                    for dir in files::read_dir(&day)? {
                        if let Some(session) = MuseAdapter::probe(&dir.join("session.jsonl"))? {
                            sessions.push(session);
                        }
                    }
                }
            }
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        cursor: Option<&Cursor>,
        _blobs: &Blobs,
    ) -> Result<NativeRead> {
        journal::read(&session.path, cursor, |leaf| {
            LogParser::new(&session.id, leaf)
        })
    }
}

impl Adapter for MuseAdapter {
    fn write(&self, export: &Export, _blobs: &Blobs) -> Result<NativeSession> {
        let created = export.created;
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        // Muse generates time ordered ids and files sessions under their local creation date
        let id = identifiers::uuid_v7();
        let mut writer = LogWriter::new(&id, created, &export.cwd.to_string_lossy());
        writer.push_events(export.events);
        let local = started.with_timezone(&Local);
        let path = self
            .root
            .join("sessions")
            .join(local.format("%Y").to_string())
            .join(local.format("%m").to_string())
            .join(local.format("%d").to_string())
            .join(&id)
            .join("session.jsonl");
        files::create_new(&path, writer.finish().as_bytes())?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    // Muse refuses to resume a session in another workspace than its own
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("muse");
        command.arg("resume").arg(&session.id).current_dir(cwd);
        command
    }
}
