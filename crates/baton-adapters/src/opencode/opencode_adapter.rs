use std::cell::OnceCell;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Source, Stamp, time,
};
use chrono::Utc;
use serde_json::{Deserializer, Value};
use tempfile::Builder;

use crate::opencode::{
    ascending_ids, bundle_parser, bundle_writer, transfer_parser, transfer_writer,
};
use crate::support::{files, program, sqlite};

/// Sessions of [OpenCode](https://github.com/anomalyco/opencode) or of its fork [Kilo Code](https://github.com/Kilo-Org/kilocode), found in their SQLite database and moved through their own export and import commands, so `opencode` or `kilo` has to be on `PATH`.
///
/// OpenCode 2 replaced those commands and their format, so the adapter asks OpenCode for its version once and picks the matching ones; Kilo follows OpenCode 1.
pub struct OpencodeAdapter {
    agent: Agent,
    data: PathBuf,
    version: OnceCell<String>,
}

impl OpencodeAdapter {
    /// An adapter for `agent`, either [`Agent::Opencode`] or [`Agent::Kilo`], whose data directory `data` holds `opencode.db` or `kilo.db`.
    pub fn new(agent: Agent, data: PathBuf) -> OpencodeAdapter {
        OpencodeAdapter {
            agent,
            data,
            version: OnceCell::new(),
        }
    }

    /// The data directory: `$XDG_DATA_HOME/opencode`, or else `~/.local/share/opencode`, and `kilo` instead of `opencode` for Kilo.
    pub fn default_data(agent: Agent) -> Result<PathBuf> {
        Ok(files::xdg_data_home()?.join(agent.name()))
    }

    fn database(&self, data: &Path) -> PathBuf {
        data.join(format!("{}.db", self.agent.name()))
    }

    // OpenCode 2 prints `opencode v2.2.5`, while OpenCode 1 and Kilo print the bare version
    fn is_v2(&self) -> Result<bool> {
        if self.agent == Agent::Kilo {
            return Ok(false);
        }
        let version = self.version()?;
        let number = version
            .trim_start_matches("opencode")
            .trim_start()
            .trim_start_matches('v');
        let major = number
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok());
        Ok(major.is_some_and(|major| major >= 2))
    }

    fn version(&self) -> Result<&str> {
        if let Some(version) = self.version.get() {
            return Ok(version);
        }
        let output = self.run(&["--version"], None)?;
        Ok(self.version.get_or_init(|| output.trim().to_owned()))
    }

    fn run(&self, args: &[&str], cwd: Option<&Path>) -> Result<String> {
        use std::io::{Read, Seek, SeekFrom};

        let name = self.agent.name();
        // The agent exits before a pipe drains, cutting large exports short, so stdout goes to a file
        let mut stdout: File = tempfile::tempfile()?;
        let mut command = program::command(name);
        command.args(args).stdout(stdout.try_clone()?);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command
            .output()
            .with_context(|| format!("running {name}, is it installed?"))?;
        if !output.status.success() {
            bail!(
                "{name} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut text = String::new();
        stdout.seek(SeekFrom::Start(0))?;
        stdout.read_to_string(&mut text)?;
        Ok(text)
    }
}

impl Source for OpencodeAdapter {
    fn agent(&self) -> Agent {
        self.agent
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(dir) => self.database(dir),
            None => self.database(&self.data),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        // OpenCode 2 keeps its sessions in a table of their own, next to those OpenCode 1 left behind
        let v2: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_v2')",
            [],
            |row| row.get(0),
        )?;
        let table = if v2 { "session_v2" } else { "session" };
        // Child sessions belong to subagents, whose results already live in their parent session
        let mut statement = connection.prepare(&format!(
            "SELECT id, title, time_updated FROM {table} WHERE parent_id IS NULL"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok(NativeSession {
                id: row.get(0)?,
                path: db.clone(),
                stamp: Stamp {
                    size: 0,
                    mtime: row.get(2)?,
                },
                title: bundle_parser::named(row.get(1)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<NativeSession>>>()?)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        blobs: &Blobs,
    ) -> Result<NativeRead> {
        // `--standalone` keeps OpenCode 2 from starting or reconfiguring the user's background service, `--pure` keeps plugins out
        if self.is_v2()? {
            let output = self.run(&["--standalone", "session", "export", &session.id], None)?;
            return transfer_parser::parse_transfer(&first_json(&output)?, blobs);
        }
        let output = self.run(&["--pure", "export", &session.id], None)?;
        bundle_parser::parse_bundle(&first_json(&output)?, blobs)
    }
}

impl Adapter for OpencodeAdapter {
    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        use std::io::Write;

        let id = ascending_ids::session_id(export.seed, time::unix_millis(export.created));
        let v2 = self.is_v2()?;
        let data = if v2 {
            transfer_writer::write_transfer(export, &id, blobs)?
        } else {
            bundle_writer::write_bundle(export, &id, self.version()?, blobs)?
        };
        let mut file = Builder::new().prefix("baton-").suffix(".json").tempfile()?;
        file.write_all(data.to_string().as_bytes())?;
        let file_path = file.path().to_string_lossy().into_owned();
        let args = if v2 {
            vec!["--standalone", "session", "import", &file_path]
        } else {
            vec!["--pure", "import", &file_path]
        };
        // The agent assigns an imported session to the project of the directory it runs in
        let output = self.run(&args, Some(export.cwd))?;
        if !output.contains(&id) {
            bail!(
                "{} import did not import {id}: {}",
                self.agent.name(),
                output.trim()
            );
        }
        Ok(NativeSession {
            id,
            path: self.database(&self.data),
            stamp: Stamp {
                size: 0,
                mtime: Utc::now().timestamp_millis(),
            },
            title: None,
        })
    }

    fn remove(&self, session: &NativeSession) -> Result<()> {
        let v2 = self.is_v2()?;
        let connection = sqlite::open_read_only(&session.path)?;
        let table = if v2 { "session_v2" } else { "session" };
        let has_children: bool = connection.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE parent_id = ?1)"),
            [&session.id],
            |row| row.get(0),
        )?;
        if has_children {
            bail!(
                "cannot remove {} session {} with child sessions",
                self.agent.name(),
                session.id
            );
        }
        let flag = if v2 { "--standalone" } else { "--pure" };
        self.run(&[flag, "session", "delete", &session.id], None)?;
        Ok(())
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command(self.agent.name());
        command.arg("--session").arg(&session.id).current_dir(cwd);
        command
    }
}

// Exports can be preceded by log lines, so the JSON starts at the first brace
fn first_json(output: &str) -> Result<Value> {
    let start = output.find('{').context("the export printed no JSON")?;
    Deserializer::from_str(&output[start..])
        .into_iter()
        .next()
        .context("the export printed no JSON")?
        .map_err(Into::into)
}
