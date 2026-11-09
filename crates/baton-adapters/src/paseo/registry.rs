use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{Agent, Direction, Link, NativeRef, NativeSession};
use serde_json::Value;

use crate::support::{files, program};

/// A supported active Paseo agent and the provider session it wraps.
#[derive(Clone, Debug)]
pub struct PaseoAgent {
    /// The Paseo agent id.
    pub id: String,
    /// The record file.
    pub path: PathBuf,
    /// The provider session handle.
    pub handle: String,
    /// The provider.
    pub provider: Agent,
    /// The working directory.
    pub cwd: String,
    /// The Paseo title.
    pub title: Option<String>,
}

/// Paseo's on-disk registrations and command-line operations.
pub struct Paseo {
    home: PathBuf,
}

impl Paseo {
    /// Uses the given Paseo home directory.
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// Finds the home selected by Paseo's environment.
    pub fn default_home() -> Result<PathBuf> {
        match files::env_dir("PASEO_HOME") {
            Some(home) if home == Path::new("~") => files::home(),
            Some(home) if home.starts_with("~/") => {
                Ok(files::home()?.join(home.strip_prefix("~/")?))
            }
            Some(home) => Ok(home),
            None => Ok(files::home()?.join(".paseo")),
        }
    }

    /// Discovers active supported agents at one directory level.
    pub fn discover(&self) -> Result<Vec<PaseoAgent>> {
        let mut result = Vec::new();
        for path in files::read_dir(&self.home.join("agents"))? {
            if path.is_dir() {
                for path in files::read_dir(&path)? {
                    self.read_record(&path, &mut result)?;
                }
            } else {
                self.read_record(&path, &mut result)?;
            }
        }
        Ok(result)
    }

    fn read_record(&self, path: &Path, agents: &mut Vec<PaseoAgent>) -> Result<()> {
        if path.extension().is_none_or(|extension| extension != "json") {
            return Ok(());
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let record: Value = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(_) => return Ok(()),
        };
        if record["internal"].as_bool() == Some(true) || !record["archivedAt"].is_null() {
            return Ok(());
        }
        let (Some(id), Some(provider_name), Some(cwd), Some(persistence)) = (
            record["id"].as_str(),
            record["provider"].as_str(),
            record["cwd"].as_str(),
            record["persistence"].as_object(),
        ) else {
            return Ok(());
        };
        if persistence.get("provider").and_then(Value::as_str) != Some(provider_name) {
            return Ok(());
        }
        let Some(handle) = persistence
            .get("nativeHandle")
            .and_then(Value::as_str)
            .or_else(|| persistence.get("sessionId").and_then(Value::as_str))
        else {
            return Ok(());
        };
        let Ok(provider) = provider_name.parse::<Agent>() else {
            return Ok(());
        };
        if !Self::supports(provider) {
            return Ok(());
        }
        agents.push(PaseoAgent {
            id: id.to_owned(),
            path: path.to_path_buf(),
            handle: handle.to_owned(),
            provider,
            cwd: cwd.to_owned(),
            title: record["title"].as_str().map(str::to_owned),
        });
        Ok(())
    }

    /// Whether Paseo supports registering this provider through baton.
    pub fn supports(agent: Agent) -> bool {
        matches!(
            agent,
            Agent::Claude | Agent::Codex | Agent::Opencode | Agent::Pi | Agent::Omp
        )
    }

    /// The Paseo handle for a provider session.
    pub fn handle(agent: Agent, native: &NativeSession) -> String {
        match agent {
            Agent::Pi | Agent::Omp => native.path.to_string_lossy().into_owned(),
            _ => native.id.clone(),
        }
    }

    /// Whether the Paseo daemon is available.
    pub fn available(&self) -> bool {
        program::command("paseo")
            .args(["ls", "--json"])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Registers a provider session, returning the Paseo id.
    pub fn register(
        &self,
        agent: Agent,
        native: &NativeSession,
        cwd: &Path,
        title: Option<&str>,
    ) -> Result<String> {
        let output = program::command("paseo")
            .arg("import")
            .arg(Self::handle(agent, native))
            .args(["--provider", agent.name(), "--cwd"])
            .arg(cwd)
            .arg("--json")
            .output()
            .context("starting paseo import")?;
        if !output.status.success() {
            bail!(
                "paseo import failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let imported: Value =
            serde_json::from_slice(&output.stdout).context("parsing paseo import output")?;
        let id = imported["agentId"]
            .as_str()
            .context("paseo import output has no agentId")?;
        if let Some(title) = title {
            let output = program::command("paseo")
                .args(["agent", "update", id, "--name", title])
                .output()
                .context("starting paseo agent update")?;
            if !output.status.success() {
                bail!(
                    "paseo agent update failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(id.to_owned())
    }

    /// Deletes a Paseo registration without removing the provider session.
    pub fn delete(&self, id: &str) -> Result<()> {
        let output = program::command("paseo")
            .args(["delete", id])
            .output()
            .context("starting paseo delete")?;
        if !output.status.success() {
            bail!(
                "paseo delete failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Builds the command that opens a Paseo agent.
    pub fn open_command(&self, id: &str) -> Command {
        let mut command = program::command("paseo");
        command.args(["agent", "open", id]);
        command
    }

    /// Makes a registry link for an agent and its provider session.
    pub fn link(
        &self,
        host: &str,
        agent: &PaseoAgent,
        native_id: String,
        direction: Direction,
    ) -> Link {
        Link {
            host: host.to_owned(),
            agent: Agent::Paseo,
            native_id: agent.id.clone(),
            provider: Some(NativeRef {
                agent: agent.provider,
                native_id,
            }),
            path: agent.path.to_string_lossy().into_owned(),
            exported_events: 0,
            direction,
            exported_digest: None,
            exported_leaf: None,
            continued: false,
            removed: false,
        }
    }

    /// Finds the persisted record of a newly registered agent.
    pub fn registered(&self, id: &str) -> Result<PaseoAgent> {
        self.discover()?
            .into_iter()
            .find(|agent| agent.id == id)
            .with_context(|| format!("paseo agent {id} has no active record"))
    }
}
