use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, time,
};
use serde_json::json;
use uuid::Uuid;

use crate::support::{files, identifiers, json, program};
use crate::vibe::fingerprint::fingerprint;
use crate::vibe::parser::{legacy, unified};
use crate::vibe::writer;

/// Sessions of [Mistral Vibe](https://github.com/mistralai/mistral-vibe): a folder per session in its session log directory, holding `messages.jsonl` and `meta.json`, and the sessions of Vibe 3's Unified store in its `unified` subdirectory
///
/// baton writes folders, which Vibe 3 imports into its store when it resumes them. A session continued there keeps the folder's id and reads as the folder's conversation followed by what Vibe 3 added
pub struct VibeAdapter {
    sessions: PathBuf,
    sessions_lock: Mutex<()>,
}

impl VibeAdapter {
    /// An adapter for the Vibe session log directory `sessions`.
    pub fn new(sessions: PathBuf) -> VibeAdapter {
        VibeAdapter {
            sessions,
            sessions_lock: Mutex::new(()),
        }
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

    // Vibe resumes the folder whose name ends in the first eight characters of the id, so those have to be unique
    fn unused_id(&self, seed: Option<Uuid>) -> Result<String> {
        let names: Vec<String> = files::read_dir(&self.sessions)?
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        loop {
            let id = identifiers::session_v4(seed).to_string();
            let short = format!("_{}", &id[..8]);
            if !names.iter().any(|name| name.ends_with(&short)) {
                return Ok(id);
            }
            if seed.is_some() {
                bail!("Vibe session ID prefix {} already exists", &id[..8]);
            }
        }
    }
}

impl Source for VibeAdapter {
    fn agent(&self) -> Agent {
        Agent::Vibe
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.sessions);
        let absolute;
        let root = if root.is_file() {
            absolute = std::path::absolute(root)?;
            absolute.parent().expect("a file lies in a session folder")
        } else {
            root
        };
        // Vibe 3 writes a `meta.json` into its own session folders too, and a read looks for the imported folder beside the store, which a relative path may not reach
        if root.join("CURRENT").is_file() {
            let dir = std::path::absolute(root)?;
            let current = dir.join("CURRENT");
            let store = dir.parent().expect("a session folder lies in its store");
            let mut sessions = unified_sessions(store)?;
            sessions.retain(|session| session.path == current);
            return Ok(sessions);
        }
        if root.join("meta.json").is_file() {
            return Ok(probe_legacy(root)?.into_iter().collect());
        }
        let unified_sessions = unified_sessions(&root.join("unified"))?;
        let mut sessions = Vec::new();
        for dir in files::read_dir(root)? {
            // A session Vibe 3 imported from a folder takes the folder's place
            if let Some(session) = probe_legacy(&dir)?
                && !unified_sessions.iter().any(|other| other.id == session.id)
            {
                sessions.push(session);
            }
        }
        sessions.extend(unified_sessions);
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        if session_id(&session.path).is_some() {
            unified::read(&session.path, blobs)
        } else {
            legacy::read(&session.path, cursor, blobs)
        }
    }
}

impl Adapter for VibeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let created = export.created;
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        let lines = writer::messages(export.events, blobs)?;
        let guard = self.sessions_lock.lock().unwrap();
        let id = self.unused_id(export.seed)?;
        let dir = self.sessions.join(format!(
            "session_{}_{}",
            started.format("%Y%m%d_%H%M%S"),
            &id[..8]
        ));
        fs::create_dir_all(&self.sessions)?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        drop(guard);
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
        if let Some(id) = session_id(&session.path) {
            bail!("Vibe session {id} lives in Vibe's own store, which baton leaves to Vibe");
        }
        let dir = session
            .path
            .parent()
            .expect("session files have a directory");
        files::remove_file(&dir.join("meta.json"))?;
        files::remove_file(&session.path)?;
        files::remove_empty_dir(dir)
    }

    // Vibe runs a resumed session in the directory it starts in, and resumes a session of its store under the id Vibe gave it
    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let id = session_id(&session.path).unwrap_or(&session.id);
        let mut command = program::command("vibe");
        command.arg("--resume").arg(id).current_dir(cwd);
        command
    }
}

fn session_id(path: &Path) -> Option<&str> {
    if path.file_name()? != "CURRENT" {
        return None;
    }

    path.parent()?.file_name()?.to_str()
}

fn probe_legacy(dir: &Path) -> Result<Option<NativeSession>> {
    let messages = dir.join("messages.jsonl");
    if !messages.is_file() {
        return Ok(None);
    }
    let Some(meta) = legacy::meta(dir) else {
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

fn unified_sessions(root: &Path) -> Result<Vec<NativeSession>> {
    let mut generations = Vec::new();
    for dir in files::read_dir(root)? {
        // The store keeps a catalog beside the session folders, and passes over a session it cannot load
        let Ok(generation) = unified::Generation::open(&dir) else {
            continue;
        };
        // A subagent's session resumes only through its parent's
        if generation.runtime["identity"]["kind"] != "subagent" {
            generations.push(generation);
        }
    }
    // Of several imports of one legacy folder, Vibe resumes the earliest
    generations.sort_by(|a, b| a.import_order().cmp(&b.import_order()));
    let mut claimed = HashSet::new();

    generations
        .iter()
        .map(|generation| {
            let id = match generation.legacy_id() {
                Some(legacy) if claimed.insert(legacy) => legacy,
                _ => generation.session_id.as_str(),
            };
            Ok(NativeSession {
                id: id.to_owned(),
                stamp: generation.stamp()?,
                path: generation.session_dir.join("CURRENT"),
                title: None,
            })
        })
        .collect()
}
