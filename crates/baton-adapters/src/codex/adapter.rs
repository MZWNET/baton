use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, journal, time,
};
use chrono::Local;
use serde_json::{Value, json};

use crate::codex::parser::RolloutParser;
use crate::codex::writer::RolloutWriter;
use crate::support::{files, identifiers, json, program};

/// Rollouts of [Codex](https://github.com/openai/codex): `sessions/YYYY/MM/DD/rollout-*.jsonl` under its home directory, plain or zstd compressed, with titles in `session_index.jsonl`.
///
/// Both legacy rollouts and paginated ones, whose records carry ordinals, are read; a thread rollback moves the active branch back. Subagent threads are skipped because their results already live in the parent thread.
pub struct CodexAdapter {
    root: PathBuf,
    index_lock: Mutex<()>,
}

impl CodexAdapter {
    /// An adapter for the Codex home directory `root`.
    pub fn new(root: PathBuf) -> CodexAdapter {
        CodexAdapter {
            root,
            index_lock: Mutex::new(()),
        }
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

    fn lock_index(&self) -> Result<File> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join("session_index.jsonl.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        lock.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(lock)
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
        let head = files::head(&session.path, 1)?;
        let provider = head
            .first()
            .and_then(|record| json::string(&record["payload"]["model_provider"]));

        journal::read(&session.path, cursor, |leaf| {
            RolloutParser::new(blobs, leaf, provider.clone())
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
        let _index = export.title.map(|_| self.index_lock.lock().unwrap());
        let _lock = if export.title.is_some() {
            Some(self.lock_index()?)
        } else {
            None
        };
        // A seeded export recreated after losing only its rollout is already indexed
        let title = if let Some(title) = export.title {
            (CodexAdapter::titles(&self.root)?
                .get(&id)
                .map(String::as_str)
                != Some(title))
            .then_some(title)
        } else {
            None
        };
        files::create_new(&path, writer.finish().as_bytes())?;
        let result = (|| {
            if let Some(title) = title {
                let row = format!(
                    "{}\n",
                    json!({"id": id, "thread_name": title, "updated_at": time::now()})
                );
                let mut index = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.root.join("session_index.jsonl"))?;
                index.write_all(row.as_bytes())?;
            }
            Stamp::of(&path)
        })();
        let stamp = match result {
            Ok(stamp) => stamp,
            Err(error) => {
                files::remove_file(&path)
                    .with_context(|| format!("cleaning {} after {error:#}", path.display()))?;
                return Err(error);
            }
        };
        Ok(NativeSession {
            id,
            stamp,
            path,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        use std::io::Write;

        let _index = self.index_lock.lock().unwrap();
        let _lock = self.lock_index()?;
        let index = self.root.join("session_index.jsonl");
        let scratch = match fs::read(&index) {
            Ok(bytes) => {
                let mut retained = Vec::with_capacity(bytes.len());
                for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                    let content = line.strip_suffix(b"\n").unwrap_or(line);
                    if content.iter().all(u8::is_ascii_whitespace) {
                        retained.extend_from_slice(line);
                        continue;
                    }
                    let row: Value = serde_json::from_slice(content)?;
                    if row["id"].as_str() != Some(session.id.as_str()) {
                        retained.extend_from_slice(line);
                    }
                }
                if retained.len() != bytes.len() {
                    let mut scratch = tempfile::NamedTempFile::new_in(&self.root)?;
                    scratch
                        .as_file()
                        .set_permissions(fs::metadata(&index)?.permissions())?;
                    scratch.write_all(&retained)?;
                    Some(scratch)
                } else {
                    None
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        files::remove_file(&session.path)?;
        if let Some(scratch) = scratch {
            scratch.persist(&index)?;
        }
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("codex");
        command.arg("resume").arg(&session.id).current_dir(cwd);
        command
    }
}
