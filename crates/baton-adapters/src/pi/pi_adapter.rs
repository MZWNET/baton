use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use baton_core::{
    Adapter, Agent, Blobs, Cursor, Export, NativeRead, NativeSession, Stamp, journal, time,
};
use serde_json::json;

use crate::pi::journal_parser::JournalParser;
use crate::pi::journal_writer::{self, JournalWriter};
use crate::support::{files, identifiers, program};

/// Sessions of [pi](https://github.com/earendil-works/pi) or of its fork [Oh My Pi](https://github.com/can1357/oh-my-pi), which share one journal format.
///
/// Oh My Pi journals start with a fixed-width title slot and keep images as separate blob files; pi journals start with the session header and inline their images. Both form a tree of entries linked by `parentId`, read along the branch that ends at the last entry.
pub struct PiAdapter {
    agent: Agent,
    root: PathBuf,
}

impl PiAdapter {
    /// An adapter for `agent`, either [`Agent::Pi`] or [`Agent::Omp`], whose agent directory is `root`.
    pub fn new(agent: Agent, root: PathBuf) -> PiAdapter {
        PiAdapter { agent, root }
    }

    /// The agent directory: `PI_CODING_AGENT_DIR`, which both agents honor, or else `~/.pi/agent` for pi and `~/.omp/agent` for Oh My Pi.
    pub fn default_root(agent: Agent) -> Result<PathBuf> {
        if let Some(root) = files::env_dir("PI_CODING_AGENT_DIR") {
            return Ok(root);
        }
        let dir = if agent == Agent::Omp { ".omp" } else { ".pi" };
        Ok(files::home()?.join(dir).join("agent"))
    }

    fn is_omp(&self) -> bool {
        self.agent == Agent::Omp
    }

    // A journal belongs to Oh My Pi exactly when it starts with a title slot, which tells the two apart in a shared directory
    fn probe(&self, file: &Path) -> Result<Option<NativeSession>> {
        let head = files::head(file, 2)?;
        let slotted = head.first().is_some_and(|first| first["type"] == "title");
        let header = head
            .get(usize::from(slotted))
            .filter(|header| header["type"] == "session" && slotted == self.is_omp());
        let Some(id) = header.and_then(|header| header["id"].as_str()) else {
            return Ok(None);
        };
        Ok(Some(NativeSession {
            id: id.to_owned(),
            path: file.to_path_buf(),
            stamp: Stamp::of(file)?,
            title: None,
        }))
    }

    // The directory a session of `cwd` goes into; Oh My Pi shortens paths inside the temporary directory, and then those inside the home directory, which holds the temporary one on Windows
    fn bucket(&self, cwd: &str) -> String {
        let escape = |path: &str| path.replace(['/', '\\', ':'], "-");
        if self.is_omp() {
            let relative = |root: &Path| {
                Path::new(cwd)
                    .strip_prefix(root)
                    .ok()
                    .map(|relative| escape(&relative.to_string_lossy()))
            };
            if let Some(relative) = relative(&env::temp_dir()) {
                if relative.is_empty() {
                    return "-tmp".into();
                }
                return format!("-tmp-{relative}");
            }
            if let Some(relative) = files::home().ok().and_then(|home| relative(&home)) {
                return format!("-{relative}");
            }
        }
        let path = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
        format!("--{}--", escape(path))
    }
}

impl Adapter for PiAdapter {
    fn agent(&self) -> Agent {
        self.agent
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        if root.is_file() {
            return Ok(self.probe(root)?.into_iter().collect());
        }
        let mut sessions = Vec::new();
        for bucket in files::read_dir(&root.join("sessions"))? {
            for file in files::read_dir(&bucket)? {
                if file
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && let Some(session) = self.probe(&file)?
                {
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
        // Blob references point into the agent directory the journal lives in, which may not be the default one
        let blob_dir = session
            .path
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "sessions"))
            .and_then(Path::parent)
            .map(|root| root.join("blobs"));
        journal::read(&session.path, cursor, |leaf| {
            JournalParser::new(blobs, blob_dir.clone(), leaf)
        })
    }

    fn write(&self, export: &Export, blobs: &Blobs) -> Result<NativeSession> {
        let id = identifiers::uuid_v7();
        let created = export.created;
        let cwd = export.cwd.to_string_lossy();
        let Some(started) = time::parse(created) else {
            bail!("session creation time `{created}` is not RFC 3339");
        };
        let file = format!("{}_{id}.jsonl", started.format("%Y-%m-%dT%H-%M-%S-%3fZ"));
        let path = self
            .root
            .join("sessions")
            .join(self.bucket(&cwd))
            .join(file);
        let blob_dir = self.is_omp().then(|| self.root.join("blobs"));
        let mut writer = JournalWriter::new(blobs, blob_dir);
        if self.is_omp() {
            let title = export.title.unwrap_or_default();
            writer.push_raw(&journal_writer::title_slot(title, created));
        }
        writer.push_line(
            &json!({"type": "session", "version": 3, "id": id, "timestamp": created, "cwd": cwd}),
        );
        writer.push_events(export.events)?;
        // Oh My Pi keeps the title in its slot, pi in a trailing session_info entry
        if !self.is_omp()
            && let Some(title) = export.title
        {
            let timestamp = export
                .events
                .last()
                .map_or(created, |event| &event.timestamp);
            writer.push_session_info(title, timestamp);
        }
        files::create_new(&path, writer.finish().as_bytes())?;
        Ok(NativeSession {
            id,
            stamp: Stamp::of(&path)?,
            path,
            title: None,
        })
    }

    fn resume_command(&self, session: &NativeSession, cwd: &Path) -> Command {
        let mut command = program::command(self.agent.name());
        let flag = if self.is_omp() {
            "--resume"
        } else {
            "--session"
        };
        command.arg(flag).arg(&session.path).current_dir(cwd);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_follow_the_naming_of_pi_and_omp() {
        let pi = PiAdapter::new(Agent::Pi, PathBuf::new());
        assert_eq!(pi.bucket(r"C:\work\demo"), "--C--work-demo--");
        assert_eq!(
            pi.bucket("//server/share"),
            "---server-share--",
            "pi strips only one leading separator"
        );
        let omp = PiAdapter::new(Agent::Omp, PathBuf::new());
        let scratch = env::temp_dir().join("demo");
        assert_eq!(
            omp.bucket(&scratch.to_string_lossy()),
            "-tmp-demo",
            "Oh My Pi checks the temporary directory before the home directory, which holds it on Windows"
        );
    }
}
