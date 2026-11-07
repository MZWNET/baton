use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal, time,
};
use chrono::Local;
use serde_json::{Value, json};

use crate::codex::rollout_parser::RolloutParser;
use crate::codex::rollout_writer::RolloutWriter;
use crate::support::{files, identifiers, json, program};

/// Rollouts of [Codex](https://github.com/openai/codex): `sessions/YYYY/MM/DD/rollout-*.jsonl` under its home directory, plain or zstd compressed, with titles in `session_index.jsonl`.
///
/// Both legacy rollouts and paginated ones, whose records carry ordinals, are read; a thread rollback moves the active branch back. Subagent threads are skipped because their results already live in the parent thread.
pub struct CodexAdapter {
    root: PathBuf,
}

impl CodexAdapter {
    /// An adapter for the Codex home directory `root`.
    pub fn new(root: PathBuf) -> CodexAdapter {
        CodexAdapter { root }
    }

    /// The Codex home directory: `CODEX_HOME`, or else `~/.codex`.
    pub fn default_root() -> Result<PathBuf> {
        match files::env_dir("CODEX_HOME") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".codex")),
        }
    }

    fn probe(file: &Path) -> Result<Option<NativeSession>> {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let rollout = name.starts_with("rollout-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"));
        if !rollout {
            return Ok(None);
        }
        let head = files::head(file, 1)?;
        let Some(meta) = head.first().filter(|first| first["type"] == "session_meta") else {
            return Ok(None);
        };
        let payload = &meta["payload"];
        if payload["thread_source"] == "subagent" || payload["source"].get("subagent").is_some() {
            return Ok(None);
        }
        let Some(id) = json::string(&payload["id"]) else {
            return Ok(None);
        };
        Ok(Some(NativeSession {
            id,
            path: file.to_path_buf(),
            stamp: Stamp::of(file)?,
            title: None,
        }))
    }

    // Titles live in an append-only index where a later line renames the thread again
    fn titles(root: &Path) -> Result<HashMap<String, String>> {
        let path = root.join("session_index.jsonl");
        if !path.is_file() {
            return Ok(HashMap::new());
        }
        let bytes = fs::read(&path)?;
        let mut titles = HashMap::new();
        for line in journal::lines(&bytes) {
            let Ok(row) = serde_json::from_slice::<Value>(line.bytes) else {
                continue;
            };
            if let (Some(id), Some(title)) =
                (json::string(&row["id"]), json::string(&row["thread_name"]))
            {
                titles.insert(id, title);
            }
        }
        Ok(titles)
    }

    // Codex refuses to resume a rollout recorded for another provider than the configured one
    fn provider(&self) -> Result<String> {
        let path = self.root.join("config.toml");
        if !path.is_file() {
            return Ok("openai".into());
        }
        let config = fs::read_to_string(&path)?;
        for line in config.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                break;
            }
            let value = line
                .strip_prefix("model_provider")
                .and_then(|rest| rest.trim_start().strip_prefix('='));
            if let Some(provider) = value.and_then(|value| value.trim().split(['"', '\'']).nth(1)) {
                return Ok(provider.to_owned());
            }
        }
        Ok("openai".into())
    }
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for path in files::read_dir(dir)? {
        if path.is_dir() {
            walk(&path, found)?;
        } else {
            found.push(path);
        }
    }
    Ok(())
}

impl Source for CodexAdapter {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(CodexAdapter::probe(root)?.into_iter().collect());
        }
        let mut rollouts = Vec::new();
        walk(&root.join("sessions"), &mut rollouts)?;
        walk(&root.join("archived_sessions"), &mut rollouts)?;
        let mut sessions: HashMap<String, NativeSession> = HashMap::new();
        for rollout in rollouts {
            let Some(session) = CodexAdapter::probe(&rollout)? else {
                continue;
            };
            // A rollout can briefly exist both plain and compressed, or both active and archived
            let newer = sessions
                .get(&session.id)
                .is_none_or(|known| known.stamp.mtime < session.stamp.mtime);
            if newer {
                sessions.insert(session.id.clone(), session);
            }
        }
        let titles = CodexAdapter::titles(root)?;
        let mut found = Vec::with_capacity(sessions.len());
        for mut session in sessions.into_values() {
            session.title = titles.get(&session.id).cloned();
            found.push(session);
        }
        Ok(found)
    }

    fn read(
        &self,
        session: &NativeSession,
        cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        journal::read(&session.path, cursor, |leaf| {
            RolloutParser::new(blobs, leaf)
        })
    }
}

impl Adapter for CodexAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        use std::io::Write;

        let created = export.created;
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        let id =
            identifiers::session_v7(export.seed, started.timestamp_millis() as u64).to_string();
        // Codex names rollouts after local time
        let local = started.with_timezone(&Local);
        let path = self
            .root
            .join("sessions")
            .join(local.format("%Y").to_string())
            .join(local.format("%m").to_string())
            .join(local.format("%d").to_string())
            .join(format!(
                "rollout-{}-{id}.jsonl",
                local.format("%Y-%m-%dT%H-%M-%S")
            ));
        let mut writer = RolloutWriter::new(blobs);
        writer.push_meta(&id, created, export.cwd, &self.provider()?);
        writer.push_events(export.events)?;
        files::create_new(&path, writer.finish().as_bytes())?;
        if let Some(title) = export.title {
            let row = json!({"id": id, "thread_name": title, "updated_at": time::now()});
            let mut index = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join("session_index.jsonl"))?;
            writeln!(index, "{row}")?;
        }
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let index = self.root.join("session_index.jsonl");
        let bytes = match fs::read(&index) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_slice(line)?;
            if row["id"].as_str() == Some(session.id.as_str()) {
                bail!(
                    "cannot safely remove Codex session {} from the shared title index",
                    session.id
                );
            }
        }
        files::remove_file(&session.path)
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("codex");
        command.arg("resume").arg(&session.id).current_dir(cwd);
        command
    }
}
