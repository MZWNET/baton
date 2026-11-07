use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, Info, NativeRead, NativeSession, Source, Stamp, journal,
    time,
};
use serde_json::{Value, json};

use crate::kimi::wire_parser::WireParser;
use crate::kimi::wire_writer::WireWriter;
use crate::support::{files, identifiers, json, program};

/// Sessions of [Kimi Code](https://github.com/MoonshotAI/kimi-code): `sessions/<workspace>/session_<uuid>/` under its home directory, the main agent's journal `agents/main/wire.jsonl` next to `state.json`.
///
/// An undo in the journal takes back the latest user turns; child sessions and the journals of subagents stay out.
pub struct KimiAdapter {
    root: PathBuf,
}

impl KimiAdapter {
    /// An adapter for the Kimi Code home directory `root`.
    pub fn new(root: PathBuf) -> KimiAdapter {
        KimiAdapter { root }
    }

    /// The Kimi Code home directory: `KIMI_CODE_HOME`, or else `~/.kimi-code`.
    pub fn default_root() -> Result<PathBuf> {
        match files::env_dir("KIMI_CODE_HOME") {
            Some(root) => Ok(root),
            None => Ok(files::home()?.join(".kimi-code")),
        }
    }

    fn probe(wire: &Path) -> Result<Option<NativeSession>> {
        let Some(dir) = wire.ancestors().nth(3) else {
            return Ok(None);
        };
        let Some(state) = state(dir) else {
            return Ok(None);
        };
        let Some(id) = json::string(&state["id"]) else {
            return Ok(None);
        };
        if state["custom"]["child_session_kind"] == "child" || !wire.is_file() {
            return Ok(None);
        }
        Ok(Some(NativeSession {
            id,
            path: wire.to_path_buf(),
            stamp: Stamp::of(wire)?,
            title: title(&state),
        }))
    }
}

fn state(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(dir.join("state.json")).ok()?).ok()
}

// Kimi titles an untitled session `New Session` until its first prompt names it
fn title(state: &Value) -> Option<String> {
    json::string(&state["title"])
        .or_else(|| json::string(&state["customTitle"]))
        .filter(|title| !title.trim().is_empty() && title != "New Session")
}

// Kimi files the sessions of a directory under a slug of its last component and a hash of the whole path
fn workspace(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let base = normalized
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let mut slug = String::new();
    for char in base.chars() {
        if char.is_ascii_lowercase() || char.is_ascii_digit() || matches!(char, '.' | '_' | '-') {
            slug.push(char);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = match slug.trim_matches('-') {
        "" | "." | ".." => "workspace",
        slug => slug,
    };
    format!(
        "wd_{slug}_{}",
        &identifiers::sha256_hex(normalized.as_bytes())[..12]
    )
}

impl Source for KimiAdapter {
    fn agent(&self) -> Agent {
        Agent::Kimi
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(KimiAdapter::probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for workspace in files::read_dir(&root.join("sessions"))? {
            for dir in files::read_dir(&workspace)? {
                let wire = dir.join("agents").join("main").join("wire.jsonl");
                if let Some(session) = KimiAdapter::probe(&wire)? {
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
        let main = session
            .path
            .parent()
            .expect("journals live in an agent directory");
        let dir = main
            .ancestors()
            .nth(2)
            .expect("agent directories live in a session directory");
        let state =
            state(dir).with_context(|| format!("reading {}", dir.join("state.json").display()))?;
        let mut read = journal::read(&session.path, cursor, |leaf| {
            WireParser::new(blobs, main.join("blobs"), leaf)
        })?;
        let created = match &state["createdAt"] {
            Value::String(created) => time::normalize(created),
            created => created.as_i64().and_then(time::from_unix_millis),
        };
        read.info = Info {
            cwd: json::string(&state["cwd"])
                .or_else(|| json::string(&state["workDir"]))
                .or_else(|| json::string(&state["custom"]["cwd"])),
            title: title(&state),
            created,
        };
        Ok(read)
    }
}

impl Adapter for KimiAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = format!("session_{}", identifiers::session_v4(export.seed));
        // Kimi spells a Windows working directory with forward slashes, and refuses a session recorded for another directory
        let cwd = export.cwd.to_string_lossy().replace('\\', "/");
        let created = time::unix_millis(export.created);
        let mut writer = WireWriter::new(blobs, created);
        writer.push_events(export.events)?;
        let dir = self.root.join("sessions").join(workspace(&cwd)).join(&id);
        let main = dir.join("agents").join("main");
        let mut state = json!({
            "id": id,
            "version": 2,
            "isCustomTitle": export.title.is_some(),
            "cwd": cwd,
            "createdAt": created,
            "updatedAt": writer.updated().max(created),
            "archived": false,
            "agents": {"main": {"homedir": main.to_string_lossy().replace('\\', "/"), "type": "main"}},
            "custom": {},
            "lastTurnReason": "completed",
        });
        if let Some(title) = export.title {
            state["title"] = json!(title);
            state["titleKind"] = json!("custom");
        }
        if let Some(prompt) = writer.last_prompt() {
            state["lastPrompt"] = json!(prompt);
        }
        let wire = main.join("wire.jsonl");
        files::create_new(&wire, writer.finish().as_bytes())?;
        // Kimi only sees a session once its state exists, so it is written last
        files::create_new(&dir.join("state.json"), state.to_string().as_bytes())?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&wire)?,
            path: wire,
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let main = session
            .path
            .parent()
            .expect("session files have a directory");
        let agents = main.parent().expect("agent directories have a parent");
        let dir = agents.parent().expect("agent directories have a session");
        files::remove_file(&dir.join("state.json"))?;
        files::remove_file(&session.path)?;
        files::remove_empty_dir(main)?;
        files::remove_empty_dir(agents)?;
        files::remove_empty_dir(dir)
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("kimi");
        command.arg("--session").arg(&session.id).current_dir(cwd);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_follow_kimi_naming() {
        assert_eq!(workspace("/fixture/work"), "wd_work_84f4a13a9723");
        assert_eq!(
            workspace("/home/alice/My Project"),
            "wd_my-project_580d6c40994b"
        );
        assert_eq!(workspace(r"C:\Users\Alice\proj\"), "wd_proj_b43ee2249cf1");
        assert_eq!(workspace("/"), "wd_workspace_e3b0c44298fc");
    }
}
