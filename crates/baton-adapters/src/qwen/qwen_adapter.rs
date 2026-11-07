use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal,
};
use uuid::Uuid;

use crate::qwen::record_parser::RecordParser;
use crate::qwen::record_writer::RecordWriter;
use crate::support::{files, identifiers, program};

/// Chats of [Qwen Code](https://github.com/QwenLM/qwen-code): `projects/<dashed project path>/chats/<session uuid>.jsonl` under its runtime directory.
///
/// Records form a tree through `parentUuid`, read along the branch that ends at the last record; a rewind starts a new branch.
pub struct QwenAdapter {
    root: PathBuf,
}

impl QwenAdapter {
    /// An adapter for the Qwen Code runtime directory `root`.
    pub fn new(root: PathBuf) -> QwenAdapter {
        QwenAdapter { root }
    }

    /// The runtime directory: `QWEN_RUNTIME_DIR`, or else `QWEN_HOME`, or else `~/.qwen`.
    pub fn default_root() -> Result<PathBuf> {
        if let Some(root) =
            files::env_dir("QWEN_RUNTIME_DIR").or_else(|| files::env_dir("QWEN_HOME"))
        {
            return Ok(root);
        }
        Ok(files::home()?.join(".qwen"))
    }

    // Chats are named after their UUID; ledgers and other sidecars next to them are not
    fn probe(file: &Path) -> Result<Option<NativeSession>> {
        let Some(id) = file
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .filter(|id| Uuid::parse_str(id).is_ok())
        else {
            return Ok(None);
        };
        Ok(Some(NativeSession {
            id: id.to_owned(),
            path: file.to_path_buf(),
            stamp: Stamp::of(file)?,
            title: None,
        }))
    }

    // Qwen names a project directory after its dashed path, lowercased on Windows
    fn bucket(cwd: &str) -> String {
        if cfg!(windows) {
            files::dashed(&cwd.to_lowercase())
        } else {
            files::dashed(cwd)
        }
    }
}

impl Source for QwenAdapter {
    fn agent(&self) -> Agent {
        Agent::Qwen
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(QwenAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for project in files::read_dir(&root.join("projects"))? {
            for file in files::read_dir(&project.join("chats"))? {
                if let Some(session) = QwenAdapter::probe(&file)? {
                    sessions.push(session);
                }
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
        journal::read(&session.path, cursor, |_| RecordParser::new(blobs))
    }
}

impl Adapter for QwenAdapter {
    // Qwen resumes a chat only in the directory its first record names, and only by a version 4 UUID
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::session_v4(export.seed).to_string();
        let cwd = export.cwd.to_string_lossy();
        let path = self
            .root
            .join("projects")
            .join(QwenAdapter::bucket(&cwd))
            .join("chats")
            .join(format!("{id}.jsonl"));
        let mut writer = RecordWriter::new(blobs, &id, &cwd);
        writer.push_events(export.events)?;
        if let Some(title) = export.title {
            let timestamp = export
                .events
                .last()
                .map_or(export.created, |event| &event.timestamp);
            writer.push_title(timestamp, title);
        }
        files::create_new(&path, writer.finish().as_bytes())?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        files::remove_file(&session.path)
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("qwen");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
