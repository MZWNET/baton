use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal,
};

use crate::dsh::parser::EventParser;
use crate::dsh::project_key::project_key;
use crate::dsh::writer::EventWriter;
use crate::support::{files, identifiers, program};

/// Sessions of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness): `sessions/<project key>/<session id>/session.v4.jsonl.zstd` under its home directory.
///
/// The append-only log is read as the visible conversation.
pub struct DshAdapter {
    root: PathBuf,
}

impl DshAdapter {
    /// An adapter for the DeepSeek Harness home directory `root`.
    pub fn new(root: PathBuf) -> DshAdapter {
        DshAdapter { root }
    }

    /// The DeepSeek Harness home directory: `DSH_HOME`, or else `~/.dsh`.
    pub fn default_home() -> Result<PathBuf> {
        match files::env_dir("DSH_HOME") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".dsh")),
        }
    }

    fn probe(path: &Path) -> Result<Option<NativeSession>> {
        let headers = files::head(path, 1)?;
        let Some(header) = headers.first() else {
            return Ok(None);
        };
        let Some(id) = header["id"].as_str() else {
            return Ok(None);
        };
        if header["type"] != "session" || header["origin"] == "subagent" {
            return Ok(None);
        }
        Ok(Some(NativeSession {
            id: id.to_owned(),
            path: path.to_path_buf(),
            stamp: Stamp::of(path)?,
            title: None,
        }))
    }

    fn session_files(root: &Path) -> Result<Vec<PathBuf>> {
        let mut sessions = Vec::new();
        for project in files::read_dir(&root.join("sessions"))? {
            if !project.is_dir() {
                continue;
            }
            for dir in files::read_dir(&project)? {
                if !dir.is_dir() {
                    continue;
                }
                if let Some(path) = files::read_dir(&dir)?
                    .into_iter()
                    .filter(|path| path.is_file())
                    .filter_map(|path| generation(&path).map(|generation| (generation, path)))
                    .max_by_key(|(generation, _)| *generation)
                    .map(|(_, path)| path)
                {
                    sessions.push(path);
                }
            }
        }
        Ok(sessions)
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join("attachments/v1/objects")
    }
}

fn generation(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".zstd").unwrap_or(name);
    let stem = name.strip_suffix(".jsonl")?;
    if stem == "session" {
        Some(0)
    } else {
        stem.strip_prefix("session.v")?.parse().ok()
    }
}

impl Source for DshAdapter {
    fn agent(&self) -> Agent {
        Agent::Dsh
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(DshAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for file in DshAdapter::session_files(root)? {
            if let Some(session) = DshAdapter::probe(&file)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let sessions = session.path.ancestors().nth(3);
        let root = sessions
            .filter(|path| path.file_name().is_some_and(|name| name == "sessions"))
            .and_then(Path::parent)
            .unwrap_or_else(|| {
                session
                    .path
                    .parent()
                    .expect("session files have a directory")
            });
        let objects_dir = root.join("attachments/v1/objects");
        journal::read(&session.path, cursor, |leaf| {
            EventParser::new(blobs, objects_dir.clone(), leaf)
        })
    }
}

impl Adapter for DshAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = format!("session-{}", identifiers::session_v4(export.seed));
        let cwd = export.cwd.to_string_lossy();
        // dsh rejects a root containing sessions with the other encoding
        let plain = DshAdapter::session_files(&self.root)?
            .iter()
            .any(|path| !journal::is_compressed(path));
        let filename = if plain {
            "session.v4.jsonl"
        } else {
            "session.v4.jsonl.zstd"
        };
        let path = self
            .root
            .join("sessions")
            .join(project_key(&cwd))
            .join(&id)
            .join(filename);
        let objects_dir = self.objects_dir();
        let mut writer = EventWriter::new(blobs, &objects_dir, &id, &cwd, export.created);
        writer.push_events(export.events)?;
        if let Some(title) = export.title {
            let timestamp = export
                .events
                .last()
                .map_or(export.created, |event| &event.timestamp);
            writer.push_title(timestamp, title);
        }
        let (header, events) = writer.finish();
        if plain {
            files::create_new(&path, format!("{header}{events}").as_bytes())?;
        } else {
            let mut bytes = zstd::bulk::compress(header.as_bytes(), 1)?;
            bytes.extend(zstd::bulk::compress(events.as_bytes(), 1)?);
            files::create_new(&path, &bytes)?;
        }
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        files::remove_file(&session.path)?;
        files::remove_empty_dir(
            session
                .path
                .parent()
                .expect("session files have a directory"),
        )
    }

    fn resume_command(&self, _session: &NativeSession, cwd: &Path) -> Command {
        // dsh web lists sessions but has no option to open one by id
        let mut command = program::command("dsh");
        command.arg("web").current_dir(cwd);
        command
    }
}
