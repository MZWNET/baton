use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use baton_core::{Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Stamp};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Deserializer, Value};
use tempfile::Builder;

use crate::opencode::{ascending_ids, bundle_parser, bundle_writer};
use crate::support::{files, program};

/// Sessions of [OpenCode](https://github.com/anomalyco/opencode), found in its SQLite database and moved through its own `export` and `import` commands, so the `opencode` command has to be on `PATH`.
pub struct OpencodeAdapter {
    data: PathBuf,
}

impl OpencodeAdapter {
    /// An adapter for the OpenCode data directory `data`, which holds `opencode.db`.
    pub fn new(data: PathBuf) -> OpencodeAdapter {
        OpencodeAdapter { data }
    }

    /// The OpenCode data directory: `$XDG_DATA_HOME/opencode`, or else `~/.local/share/opencode`.
    pub fn default_data() -> Result<PathBuf> {
        let base = match files::env_dir("XDG_DATA_HOME") {
            Some(base) => base,
            None => files::home()?.join(".local").join("share"),
        };
        Ok(base.join("opencode"))
    }

    fn run(&self, args: &[&str], cwd: Option<&Path>) -> Result<String> {
        use std::io::{Read, Seek, SeekFrom};

        // OpenCode exits before a pipe drains, cutting large exports short, so stdout goes to a file
        let mut stdout: File = tempfile::tempfile()?;
        let mut command = program::command("opencode");
        command.args(args).stdout(stdout.try_clone()?);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command
            .output()
            .context("running opencode, is it installed?")?;
        if !output.status.success() {
            bail!(
                "opencode {} failed: {}",
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

impl Adapter for OpencodeAdapter {
    fn agent(&self) -> Agent {
        Agent::Opencode
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let db = match path {
            Some(path) if path.is_file() => path.to_path_buf(),
            Some(dir) => dir.join("opencode.db"),
            None => self.data.join("opencode.db"),
        };
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let connection = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", db.display()))?;
        // Child sessions belong to subagents, whose results already live in their parent session
        let mut statement = connection
            .prepare("SELECT id, title, time_updated FROM session WHERE parent_id IS NULL")?;
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
        let output = self.run(&["export", &session.id], None)?;
        let start = output
            .find('{')
            .context("opencode export printed no JSON")?;
        let bundle: Value = Deserializer::from_str(&output[start..])
            .into_iter()
            .next()
            .context("opencode export printed no JSON")??;
        bundle_parser::parse_bundle(&bundle, blobs)
    }

    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        use std::io::Write;

        let version = self.run(&["--version"], None)?.trim().to_owned();
        let id = ascending_ids::session_id();
        let bundle = bundle_writer::write_bundle(export, &id, &version, blobs)?;
        let mut file = Builder::new().prefix("baton-").suffix(".json").tempfile()?;
        file.write_all(bundle.to_string().as_bytes())?;
        let file_path = file.path().to_string_lossy().into_owned();
        // OpenCode assigns an imported session to the project of the directory it runs in
        let output = self.run(&["import", &file_path], Some(export.cwd))?;
        if !output.contains(&id) {
            bail!("opencode import did not import {id}: {}", output.trim());
        }
        Ok(NativeSession {
            id,
            path: self.data.join("opencode.db"),
            stamp: Stamp {
                size: 0,
                mtime: Utc::now().timestamp_millis(),
            },
            title: None,
        })
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command("opencode");
        command.arg("--session").arg(&session.id).current_dir(cwd);
        command
    }
}
