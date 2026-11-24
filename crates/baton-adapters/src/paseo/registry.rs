use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{Agent, Direction, Link, NativeRef, NativeSession};
use serde_json::Value;

use crate::support::{files, program};

const PROVIDERS: [Agent; 5] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Opencode,
    Agent::Pi,
    Agent::Omp,
];

/// A supported Paseo agent and the provider session it wraps
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

/// Paseo agents grouped by whether Paseo opens them
#[derive(Default)]
pub struct PaseoAgents {
    /// Agents Paseo opens
    pub active: Vec<PaseoAgent>,
    /// Agents archived or of a provider Paseo has turned off, which baton leaves alone
    pub inactive: Vec<PaseoAgent>,
}

/// Paseo's on-disk registrations and command-line operations.
pub struct Paseo {
    home: PathBuf,
    providers: Vec<Agent>,
}

impl Paseo {
    /// Uses the given Paseo home directory, reading which providers its settings turn on
    pub fn open(home: PathBuf) -> Result<Self> {
        let path = home.join("config.json");
        let settings: Value = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let providers = PROVIDERS
            .into_iter()
            .filter(|provider| {
                // Paseo ships Oh My Pi turned off
                settings["agents"]["providers"][provider.name()]["enabled"]
                    .as_bool()
                    .unwrap_or(*provider != Agent::Omp)
            })
            .collect();

        Ok(Self { home, providers })
    }

    /// The supported providers this Paseo has turned on
    pub fn providers(&self) -> &[Agent] {
        &self.providers
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

    /// Discovers supported agents at one directory level and sorts them into active and inactive ones
    pub fn discover(&self) -> Result<PaseoAgents> {
        let mut result = PaseoAgents::default();
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

    fn read_record(&self, path: &Path, agents: &mut PaseoAgents) -> Result<()> {
        if path.extension().is_none_or(|extension| extension != "json") {
            return Ok(());
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let record: Value = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(_) => return Ok(()),
        };
        if record["internal"].as_bool() == Some(true) {
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
        let agent = PaseoAgent {
            id: id.to_owned(),
            path: path.to_path_buf(),
            handle: handle.to_owned(),
            provider,
            cwd: cwd.to_owned(),
            title: record["title"].as_str().map(str::to_owned),
        };
        if record["archivedAt"].is_null() && self.providers.contains(&provider) {
            agents.active.push(agent);
        } else {
            agents.inactive.push(agent);
        }
        Ok(())
    }

    /// Whether Paseo supports registering this provider through baton.
    pub fn supports(agent: Agent) -> bool {
        PROVIDERS.contains(&agent)
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
            writer_version: 0,
            continued: false,
            removed: false,
        }
    }

    /// Reads the persisted record of a newly registered agent
    pub fn record(&self, id: &str) -> Result<PaseoAgent> {
        let filename = format!("{id}.json");
        let mut agents = PaseoAgents::default();
        for path in files::read_dir(&self.home.join("agents"))? {
            if path.is_dir() {
                let record = path.join(&filename);
                if record.is_file() {
                    self.read_record(&record, &mut agents)?;
                }
            } else if path
                .file_name()
                .is_some_and(|name| name == filename.as_str())
            {
                self.read_record(&path, &mut agents)?;
            }
        }
        agents
            .active
            .into_iter()
            .find(|agent| agent.id == id)
            .with_context(|| format!("paseo agent {id} has no active record"))
    }
}
