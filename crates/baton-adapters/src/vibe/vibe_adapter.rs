use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Source, Stamp, journal,
    time,
};
use serde_json::{Value, json};

use crate::support::{files, identifiers, json, program};
use crate::vibe::fingerprint::fingerprint;
use crate::vibe::message_parser::MessageParser;
use crate::vibe::message_writer;

/// Sessions of [Mistral Vibe](https://github.com/mistralai/mistral-vibe): a folder per session in its session log directory, holding `messages.jsonl` and `meta.json`.
///
/// Vibe 3 imports such a folder into a store of its own when it resumes it, so a session continued there does not flow back.
pub struct VibeAdapter {
    sessions: PathBuf,
}

impl VibeAdapter {
    /// An adapter for the Vibe session log directory `sessions`.
    pub fn new(sessions: PathBuf) -> VibeAdapter {
        VibeAdapter { sessions }
    }

    /// The session log directory: `VIBE_SESSION_LOGGING__SAVE_DIR`, or else `logs/session` in `VIBE_HOME` or `~/.vibe`.
    pub fn default_sessions() -> Result<PathBuf> {
        if let Some(dir) = files::env_dir("VIBE_SESSION_LOGGING__SAVE_DIR") {
            return Ok(dir);
        }
        let home = match files::env_dir("VIBE_HOME") {
            Some(home) => home,
            None => files::home()?.join(".vibe"),
        };
        Ok(home.join("logs").join("session"))
    }

    fn probe(dir: &Path) -> Result<Option<NativeSession>> {
        let messages = dir.join("messages.jsonl");
        if !messages.is_file() {
            return Ok(None);
        }
        let Some(meta) = meta(dir) else {
            return Ok(None);
        };
        let Some(id) = json::string(&meta["session_id"]) else {
            return Ok(None);
        };
        Ok(Some(NativeSession {
            id,
            stamp: Stamp::of(&messages)?,
            path: messages,
            title: json::string(&meta["title"]),
        }))
    }

    // Vibe resumes the folder whose name ends in the first eight characters of the id, so those have to be unique
    fn unused_id(&self) -> Result<String> {
        let names: Vec<String> = files::read_dir(&self.sessions)?
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        loop {
            let id = identifiers::uuid_v4();
            let short = format!("_{}", &id[..8]);
            if !names.iter().any(|name| name.ends_with(&short)) {
                return Ok(id);
            }
        }
    }
}

fn meta(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(dir.join("meta.json")).ok()?).ok()
}

impl Source for VibeAdapter {
    fn agent(&self) -> Agent {
        Agent::Vibe
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.sessions);
        if root.join("meta.json").is_file() {
            return Ok(VibeAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for dir in files::read_dir(root)? {
            if let Some(session) = VibeAdapter::probe(&dir)? {
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
        let dir = session
            .path
            .parent()
            .expect("message logs live in a session folder");
        let meta =
            meta(dir).with_context(|| format!("reading {}", dir.join("meta.json").display()))?;
        let created = meta["start_time"].as_str().and_then(time::normalize);
        let saved = meta["end_time"].as_str().and_then(time::normalize);
        let started = created.clone().unwrap_or_else(time::now);
        // Messages have no timestamps, and lines read after a cursor came with the last save
        let mut read = journal::read(&session.path, cursor, |leaf| {
            let timestamp = match leaf {
                Some(_) => saved.clone().unwrap_or_else(|| started.clone()),
                None => started.clone(),
            };
            MessageParser::new(blobs, dir, leaf, timestamp)
        })?;
        read.info = Info {
            cwd: json::string(&meta["environment"]["working_directory"]),
            title: json::string(&meta["title"]),
            created,
        };
        Ok(read)
    }
}

impl Adapter for VibeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let created = export.created;
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        let lines = message_writer::messages(export.events, blobs)?;
        let id = self.unused_id()?;
        let dir = self.sessions.join(format!(
            "session_{}_{}",
            started.format("%Y%m%d_%H%M%S"),
            &id[..8]
        ));
        fs::create_dir_all(&self.sessions)?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut messages = String::new();
        for line in &lines {
            json::push_line(&mut messages, line);
        }
        let path = dir.join("messages.jsonl");
        files::create_new(&path, messages.as_bytes())?;
        let ended = export
            .events
            .last()
            .map_or(created, |event| &event.timestamp);
        let username = env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into());
        // Vibe rejects metadata without these fields, and appends to the log only while the fingerprint matches its last line
        let meta = json!({
            "session_id": id,
            "parent_session_id": null,
            "start_time": created,
            "end_time": ended,
            "git_commit": null,
            "git_branch": null,
            "environment": {"working_directory": export.cwd.to_string_lossy()},
            "username": username,
            "child_sessions": [],
            "loops": [],
            "title": export.title,
            "title_source": if export.title.is_some() { "manual" } else { "auto" },
            "total_messages": lines.len(),
            "last_message_fingerprint": lines.last().map(fingerprint),
        });
        files::create_new(
            &dir.join("meta.json"),
            serde_json::to_string_pretty(&meta)?.as_bytes(),
        )?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let dir = session
            .path
            .parent()
            .expect("session files have a directory");
        files::remove_file(&dir.join("meta.json"))?;
        files::remove_file(&session.path)?;
        files::remove_empty_dir(dir)
    }

    // Vibe runs a resumed session in the directory it starts in
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("vibe");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
