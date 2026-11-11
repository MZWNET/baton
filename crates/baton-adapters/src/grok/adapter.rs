use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Progress, Source,
    Stamp, time,
};
use serde_json::{Value, json};

use crate::grok::writer::UpdateWriter;
use crate::grok::{bucket, parser};
use crate::support::{files, identifiers, json, program};

// Directory names of sessions are at most this long on common file systems
const NAME_MAX: usize = 255;

/// Sessions of [Grok Build](https://github.com/xai-org/grok-build): `sessions/<encoded cwd>/<session uuid>/` under its home directory, an ACP update log `updates.jsonl` with the session's facts in `summary.json`.
///
/// A rewind marker in the log takes the conversation back to an earlier prompt; subagent and other hidden sessions stay out.
pub struct GrokAdapter {
    root: PathBuf,
}

impl GrokAdapter {
    /// An adapter for the Grok home directory `root`.
    pub fn new(root: PathBuf) -> GrokAdapter {
        GrokAdapter { root }
    }

    /// The Grok home directory: `GROK_HOME`, or else `~/.grok`.
    pub fn default_root() -> Result<PathBuf> {
        match files::env_dir("GROK_HOME") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".grok")),
        }
    }

    // Grok writes `summary.json` last, so a session directory without one is unfinished
    fn probe(updates: &Path) -> Result<Option<NativeSession>> {
        let Some(dir) = updates.parent() else {
            return Ok(None);
        };
        let Ok(summary) = fs::read(dir.join("summary.json")) else {
            return Ok(None);
        };
        let Ok(summary) = serde_json::from_slice::<Value>(&summary) else {
            return Ok(None);
        };
        let hidden = match summary["hidden"].as_bool() {
            Some(hidden) => hidden,
            None => summary["session_kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("subagent")),
        };
        let Some(id) = json::string(&summary["info"]["id"]) else {
            return Ok(None);
        };
        if hidden || !updates.is_file() {
            return Ok(None);
        }
        Ok(Some(NativeSession {
            id,
            path: updates.to_path_buf(),
            stamp: Stamp::of(updates)?,
            title: parser::title(&summary),
        }))
    }
}

impl Source for GrokAdapter {
    fn agent(&self) -> Agent {
        Agent::Grok
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(GrokAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for bucket in files::read_dir(&root.join("sessions"))? {
            if !bucket.is_dir() {
                continue;
            }
            for dir in files::read_dir(&bucket)? {
                let hidden = dir
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'));
                if !hidden && let Some(session) = GrokAdapter::probe(&dir.join("updates.jsonl"))? {
                    sessions.push(session);
                }
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
        let summary = session.path.with_file_name("summary.json");
        let summary: Value = serde_json::from_slice(&fs::read(&summary)?)
            .with_context(|| format!("parsing {}", summary.display()))?;
        let info = Info {
            cwd: json::string(&summary["info"]["cwd"]),
            title: parser::title(&summary),
            created: summary["created_at"].as_str().and_then(time::normalize),
        };
        let bytes = fs::read(&session.path)
            .with_context(|| format!("reading {}", session.path.display()))?;
        let fallback = info.created.clone().unwrap_or_else(time::now);
        Ok(NativeRead {
            drafts: parser::drafts(&bytes, &fallback, blobs)?,
            info,
            continued: false,
            progress: Progress::default(),
        })
    }
}

impl Adapter for GrokAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::session_v7(export.seed, time::unix_millis(export.created) as u64)
            .to_string();
        let cwd = export.cwd.to_string_lossy().into_owned();
        let mut writer = UpdateWriter::new(blobs, &id);
        writer.push_events(export.events)?;
        let mut name = bucket::name(&cwd);
        let hashed = name.len() > NAME_MAX;
        if hashed {
            name = bucket::hashed(&cwd);
        }
        let bucket = self.root.join("sessions").join(name);
        fs::create_dir_all(&bucket)?;
        let sidecar = bucket.join(".cwd");
        if hashed && !sidecar.exists() {
            files::publish(&sidecar, cwd.as_bytes())?;
        }
        let dir = bucket.join(&id);
        let updates = dir.join("updates.jsonl");
        let lines = writer.lines();
        let updated = export
            .events
            .last()
            .map_or(export.created, |event| &event.timestamp);
        // Without the chat format version Grok starts the resumed model from an empty context
        let mut summary = json!({
            "info": {"id": id, "cwd": cwd},
            "session_summary": export.title.unwrap_or_default(),
            "created_at": export.created,
            "updated_at": updated,
            "last_active_at": updated,
            "num_messages": lines,
            "current_model_id": "grok-build",
            "chat_format_version": 1,
        });
        // A manual title is never replaced by the one Grok generates
        if let Some(title) = export.title {
            summary["generated_title"] = json!(title);
            summary["title_is_manual"] = json!(true);
        }
        let stamp = files::create_dir_with(&dir, || {
            files::create_new(&updates, writer.finish().as_bytes())?;
            let stamp = Stamp::of(&updates)?;
            files::create_new(&dir.join("summary.json"), summary.to_string().as_bytes())?;
            Ok(stamp)
        })?;
        Ok(NativeSession {
            id,
            stamp,
            path: updates,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let dir = session
            .path
            .parent()
            .expect("session files have a directory");
        files::remove_file(&dir.join("summary.json"))?;
        files::remove_file(&session.path)?;
        files::remove_empty_dir(dir)
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("grok");
        command.arg("--resume").arg(&session.id).current_dir(cwd);
        command
    }
}
