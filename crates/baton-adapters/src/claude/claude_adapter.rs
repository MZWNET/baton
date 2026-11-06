use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal,
};
use uuid::Uuid;

use crate::claude::project_key;
use crate::claude::transcript_parser::TranscriptParser;
use crate::claude::transcript_writer::TranscriptWriter;
use crate::support::{files, identifiers, program};

/// Sessions of [Claude Code](https://github.com/anthropics/claude-code): `projects/<project key>/<session uuid>.jsonl` under its configuration directory.
///
/// Records form a tree through `parentUuid`, read along the branch that ends at the last record, including the history before a compaction boundary.
pub struct ClaudeAdapter {
    root: PathBuf,
}

impl ClaudeAdapter {
    /// An adapter for the configuration directory `root`.
    pub fn new(root: PathBuf) -> ClaudeAdapter {
        ClaudeAdapter { root }
    }

    /// The configuration directory: `CLAUDE_CONFIG_DIR`, or else `~/.claude`.
    pub fn default_root() -> Result<PathBuf> {
        match files::env_dir("CLAUDE_CONFIG_DIR") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".claude")),
        }
    }

    // Sessions are named after their UUID; subagent transcripts are named `agent-<id>.jsonl` and stay out
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
}

impl Source for ClaudeAdapter {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(ClaudeAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for project in files::read_dir(&root.join("projects"))? {
            for file in files::read_dir(&project)? {
                if let Some(session) = ClaudeAdapter::probe(&file)? {
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
        journal::read(&session.path, cursor, |_| TranscriptParser::new(blobs))
    }
}

impl Adapter for ClaudeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::uuid_v4();
        let cwd = export.cwd.to_string_lossy();
        let path = self
            .root
            .join("projects")
            .join(project_key::from_cwd(&cwd))
            .join(format!("{id}.jsonl"));
        let mut writer = TranscriptWriter::new(blobs, &id, &cwd);
        writer.push_events(export.events)?;
        if let Some(title) = export.title {
            writer.push_title(title);
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
        let mut command = program::command("claude");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
