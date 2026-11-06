use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal,
};
use serde_json::json;
use uuid::Uuid;

use crate::copilot::event_parser::EventParser;
use crate::copilot::event_writer::EventWriter;
use crate::copilot::workspace;
use crate::support::{files, identifiers, program};

/// Sessions of [GitHub Copilot CLI](https://github.com/github/copilot-cli): `session-state/<session uuid>/events.jsonl` under its home directory, with the title in the `workspace.yaml` next to it.
///
/// The event log is append-only and replayed in line order; events of subagents and sessions detached from another one stay out.
pub struct CopilotAdapter {
    root: PathBuf,
}

impl CopilotAdapter {
    /// An adapter for the Copilot home directory `root`.
    pub fn new(root: PathBuf) -> CopilotAdapter {
        CopilotAdapter { root }
    }

    /// The Copilot home directory: `COPILOT_HOME`, or else `~/.copilot`.
    pub fn default_root() -> Result<PathBuf> {
        match files::env_dir("COPILOT_HOME") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".copilot")),
        }
    }

    // A session directory is named after the session's UUID and starts its log with that session
    fn probe(events: &Path) -> Result<Option<NativeSession>> {
        let Some(dir) = events.parent() else {
            return Ok(None);
        };
        let Some(id) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| Uuid::parse_str(name).is_ok())
        else {
            return Ok(None);
        };
        if !events.is_file() || dir.join(".detached").exists() {
            return Ok(None);
        }
        let head = files::head(events, 1)?;
        let started = head.first().is_some_and(|first| {
            first["type"] == "session.start"
                && first["data"]["sessionId"] == id
                && first["data"]["detachedFromSpawningParentSessionId"].is_null()
        });
        if !started {
            return Ok(None);
        }
        let title = fs::read_to_string(dir.join("workspace.yaml"))
            .ok()
            .and_then(|yaml| workspace::name(&yaml));
        Ok(Some(NativeSession {
            id: id.to_owned(),
            path: events.to_path_buf(),
            stamp: Stamp::of(events)?,
            title,
        }))
    }
}

impl Source for CopilotAdapter {
    fn agent(&self) -> Agent {
        Agent::Copilot
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(CopilotAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for dir in files::read_dir(&root.join("session-state"))? {
            if let Some(session) = CopilotAdapter::probe(&dir.join("events.jsonl"))? {
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
        journal::read(&session.path, cursor, |leaf| EventParser::new(blobs, leaf))
    }
}

impl Adapter for CopilotAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::uuid_v4();
        let cwd = export.cwd.to_string_lossy();
        let mut writer = EventWriter::new(blobs);
        writer.push_start(&id, export.created, &cwd);
        writer.push_events(export.events)?;
        let dir = self.root.join("session-state").join(&id);
        fs::create_dir_all(self.root.join("session-state"))?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let events = dir.join("events.jsonl");
        files::create_new(&events, writer.finish().as_bytes())?;
        let updated = export
            .events
            .last()
            .map_or(export.created, |event| &event.timestamp);
        let mut fields = vec![
            ("id", json!(id)),
            ("cwd", json!(cwd)),
            ("client_name", json!("github/cli")),
        ];
        // A title the user gave stops Copilot from titling the session itself
        if let Some(title) = export.title {
            fields.push(("name", json!(title)));
            fields.push(("user_named", json!(true)));
        }
        fields.push(("summary_count", json!(0)));
        fields.push(("created_at", json!(export.created)));
        fields.push(("updated_at", json!(updated)));
        files::create_new(
            &dir.join("workspace.yaml"),
            workspace::render(&fields).as_bytes(),
        )?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&events)?,
            path: events,
            title: None,
        })
    }

    // A UUID Copilot does not know starts a new empty session under it instead of failing
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("copilot");
        command
            .arg(format!("--resume={}", session.id))
            .current_dir(cwd);
        command
    }
}
