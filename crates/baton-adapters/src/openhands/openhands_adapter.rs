use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use serde_json::Value;
use uuid::Uuid;

use crate::openhands::event_parser;
use crate::openhands::event_writer::EventWriter;
use crate::support::{files, identifiers, json, program};

/// Conversations of [OpenHands CLI](https://github.com/OpenHands/OpenHands-CLI): `<conversation uuid hex>/events/event-<index>-<event id>.json` in its conversations directory, one file per event.
///
/// OpenHands keeps no title; subagent conversations are nested inside their parent's directory and stay out.
pub struct OpenhandsAdapter {
    conversations: PathBuf,
}

impl OpenhandsAdapter {
    /// An adapter for the OpenHands conversations directory `conversations`.
    pub fn new(conversations: PathBuf) -> OpenhandsAdapter {
        OpenhandsAdapter { conversations }
    }

    /// The conversations directory: `OPENHANDS_CONVERSATIONS_DIR`, or else `conversations` in `OPENHANDS_PERSISTENCE_DIR` or `~/.openhands`.
    pub fn default_conversations() -> Result<PathBuf> {
        if let Some(dir) = files::env_dir("OPENHANDS_CONVERSATIONS_DIR") {
            return Ok(dir);
        }
        let persistence = match files::env_dir("OPENHANDS_PERSISTENCE_DIR") {
            Some(dir) => dir,
            None => files::home()?.join(".openhands"),
        };
        Ok(persistence.join("conversations"))
    }

    // A conversation directory is named after the conversation's UUID without dashes
    fn probe(dir: &Path) -> Result<Option<NativeSession>> {
        let Some(id) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.len() == 32)
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            return Ok(None);
        };
        let files = event_files(dir)?;
        if files.is_empty() {
            return Ok(None);
        }
        // Events are added as new files and never rewritten
        let added = Stamp::of(&dir.join("events"))?;
        Ok(Some(NativeSession {
            id: id.hyphenated().to_string(),
            path: dir.to_path_buf(),
            stamp: Stamp {
                size: files.len() as u64,
                mtime: added.mtime,
            },
            title: None,
        }))
    }
}

// The event files in index order, up to the first missing index, where OpenHands stops reading too
fn event_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for path in files::read_dir(&dir.join("events"))? {
        let index = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.ends_with(".json"))
            .and_then(|name| name.strip_prefix("event-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|index| index.parse::<usize>().ok());
        if index == Some(found.len()) {
            found.push(path);
        } else if index.is_some() {
            break;
        }
    }
    Ok(found)
}

impl Source for OpenhandsAdapter {
    fn agent(&self) -> Agent {
        Agent::Openhands
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.conversations);
        if root.join("events").is_dir() {
            return Ok(OpenhandsAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for dir in files::read_dir(root)? {
            if let Some(session) = OpenhandsAdapter::probe(&dir)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        let mut created = None;
        let mut fallback = None;
        let mut drafts = Vec::new();
        for path in event_files(&session.path)? {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let event: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            let fallback = fallback.get_or_insert_with(|| {
                created = event["timestamp"]
                    .as_str()
                    .and_then(event_parser::timestamp);
                created.clone().unwrap_or_else(time::now)
            });
            drafts.extend(event_parser::drafts(&event, fallback, blobs)?);
        }
        // The workspace is recorded only once OpenHands has run the conversation, and moves to wherever it resumes
        let state = fs::read(session.path.join("base_state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let cwd = state.and_then(|state| json::string(&state["workspace"]["working_dir"]));
        Ok(NativeRead {
            info: Info {
                cwd,
                title: None,
                created,
            },
            drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for OpenhandsAdapter {
    // Without `base_state.json` OpenHands builds a fresh state around the events when it resumes them
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::session_v4(export.seed);
        let mut writer = EventWriter::new(blobs);
        writer.push_events(export.created, export.events)?;
        let dir = self.conversations.join(id.simple().to_string());
        fs::create_dir_all(&self.conversations)?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for (index, event) in writer.finish().iter().enumerate() {
            let event_id = event["id"].as_str().expect("written events have ids");
            let path = dir
                .join("events")
                .join(format!("event-{index:05}-{event_id}.json"));
            files::create_new(&path, format!("{event}\n").as_bytes())?;
        }
        Ok(OpenhandsAdapter::probe(&dir)?.expect("a written conversation is a conversation"))
    }

    fn remove(&self, _session: &NativeSession) -> Result<()> {
        bail!("OpenHands cannot safely identify which event files were created by an export")
    }

    // The workspace is wherever OpenHands runs, so it resumes in the session's directory
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("openhands");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
