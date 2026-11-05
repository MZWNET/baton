use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::storage::Blobs;
use crate::{Event, Session, SessionMeta, journal};

/// The data directory: `sessions/<id>/events.jsonl` and `sessions/<id>/meta.json` per session, plus `blobs/`.
///
/// Event logs are only ever appended to and blobs only added, so the directory suits incremental sync tools; `meta.json` is replaced atomically.
pub struct Store {
    root: PathBuf,
}

// Events read from a log, and the byte offset just past the last complete line
pub(crate) struct EventLog {
    pub(crate) events: Vec<Event>,
    pub(crate) end: u64,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Store {
        Store { root }
    }

    /// Loads a session with all of its events.
    pub fn load(&self, id: &str) -> Result<Session> {
        let meta = self.meta(id)?;
        let log = self.events_from(id, 0)?;
        Ok(Session::new(meta, log.events))
    }

    pub(crate) fn blobs(&self) -> Blobs {
        Blobs::new(self.root.join("blobs"))
    }

    pub(crate) fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join("sessions").join(id).join("meta.json")
    }

    pub(crate) fn events_path(&self, id: &str) -> PathBuf {
        self.root.join("sessions").join(id).join("events.jsonl")
    }

    pub(crate) fn ids(&self) -> Result<Vec<String>> {
        let dir = self.root.join("sessions");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.path().join("meta.json").is_file() {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(ids)
    }

    pub(crate) fn meta(&self, id: &str) -> Result<SessionMeta> {
        let path = self.meta_path(id);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub(crate) fn events_from(&self, id: &str, offset: u64) -> Result<EventLog> {
        use std::io::{Read, Seek, SeekFrom};

        let path = self.events_path(id);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(EventLog {
                    events: Vec::new(),
                    end: 0,
                });
            }
            Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut log = EventLog {
            events: Vec::new(),
            end: offset,
        };
        for line in journal::lines(&bytes) {
            log.end = offset + line.end as u64;
            if let Ok(event) = serde_json::from_slice(line.bytes) {
                log.events.push(event);
            }
        }
        Ok(log)
    }

    pub(crate) fn create(&self, meta: &SessionMeta, events: &[Event]) -> Result<()> {
        fs::create_dir_all(self.root.join("sessions").join(&meta.id))?;
        self.append(&meta.id, events)?;
        self.save_meta(meta)
    }

    pub(crate) fn append(&self, id: &str, events: &[Event]) -> Result<()> {
        use std::io::Write;

        let mut lines = String::new();
        for event in events {
            lines.push_str(&serde_json::to_string(event)?);
            lines.push('\n');
        }
        let path = self.events_path(id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(lines.as_bytes())?;
        Ok(())
    }

    pub(crate) fn save_meta(&self, meta: &SessionMeta) -> Result<()> {
        let mut text = serde_json::to_string_pretty(meta)?;
        text.push('\n');
        write_atomic(&self.meta_path(&meta.id), text.as_bytes())
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().expect("store paths end in a file name");
    let temporary = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
}
