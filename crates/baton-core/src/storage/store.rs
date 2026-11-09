use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::model::Record;
use crate::storage::Blobs;
use crate::{Event, Link, Session, SessionMeta};

/// The write-once data directory of compressed event segments, records and blobs.
pub struct Store {
    root: PathBuf,
    host: String,
}

pub(crate) struct SessionFiles {
    pub id: String,
    pub events: Vec<String>,
    pub records: Vec<String>,
}

impl Store {
    pub(crate) fn new(root: PathBuf, host: String) -> Store {
        Store { root, host }
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Loads a session with all of its events.
    pub fn load(&self, id: &str) -> Result<Session> {
        let events = self.events(id)?;
        let records = self.records(id)?;
        let meta = self.metadata(id, &records, &events);
        Ok(Session::new(meta, events))
    }

    pub(crate) fn blobs(&self) -> Blobs {
        Blobs::new(self.root.join("blobs"))
    }

    pub(crate) fn session_files(&self) -> Result<Vec<SessionFiles>> {
        let dir = self.root.join("sessions");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
        };
        let mut sessions = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let records = self.files(&id, "records")?;
            if !records.is_empty() {
                let events = self.files(&id, "events")?;
                sessions.push(SessionFiles {
                    id,
                    events,
                    records,
                });
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    pub(crate) fn files(&self, id: &str, kind: &str) -> Result<Vec<String>> {
        let dir = self.root.join("sessions").join(id).join(kind);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
        };
        let suffix = if kind == "events" {
            ".jsonl.zst"
        } else {
            ".json"
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(suffix) && entry.file_type()?.is_file() {
                names.push(name);
            }
        }
        names.sort_by(|a, b| record_order(a).cmp(&record_order(b)));
        Ok(names)
    }

    pub(crate) fn segment(&self, id: &str, name: &str) -> Result<Vec<Event>> {
        let path = self
            .root
            .join("sessions")
            .join(id)
            .join("events")
            .join(name);
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let bytes = zstd::stream::decode_all(file)
            .with_context(|| format!("decoding {}", path.display()))?;

        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice(line).with_context(|| format!("parsing {}", path.display()))
            })
            .collect()
    }

    pub(crate) fn record_meta(&self, id: &str) -> Result<SessionMeta> {
        Ok(self.metadata(id, &self.records(id)?, &[]))
    }

    pub(crate) fn link_versions(&self, id: &str) -> Result<Vec<(String, Link)>> {
        Ok(self
            .records(id)?
            .into_iter()
            .filter_map(|(name, record)| record.link.map(|link| (name, link)))
            .collect())
    }

    pub(crate) fn index_records(
        &self,
        id: &str,
        names: &[String],
    ) -> Result<(SessionMeta, Vec<(String, Link)>)> {
        let records = self.read_records(id, names)?;
        let meta = self.metadata(id, &records, &[]);
        let links = records
            .into_iter()
            .filter_map(|(name, record)| record.link.map(|link| (name, link)))
            .collect();
        Ok((meta, links))
    }

    fn records(&self, id: &str) -> Result<Vec<(String, Record)>> {
        self.read_records(id, &self.files(id, "records")?)
    }

    fn read_records(&self, id: &str, names: &[String]) -> Result<Vec<(String, Record)>> {
        names
            .iter()
            .map(|name| {
                let path = self
                    .root
                    .join("sessions")
                    .join(id)
                    .join("records")
                    .join(name);
                let bytes =
                    fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                let record: Record = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok((name.clone(), record))
            })
            .collect()
    }

    fn events(&self, id: &str) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut ids = HashSet::new();
        for name in self.files(id, "events")? {
            events.extend(
                self.segment(id, &name)?
                    .into_iter()
                    .filter(|event| ids.insert(event.id.clone())),
            );
        }
        Ok(events)
    }

    fn metadata(&self, id: &str, records: &[(String, Record)], events: &[Event]) -> SessionMeta {
        let mut meta = SessionMeta {
            id: id.to_owned(),
            title: None,
            cwd: None,
            project: None,
            created: events
                .iter()
                .map(|event| event.timestamp.as_str())
                .min()
                .unwrap_or_default()
                .to_owned(),
            updated: events
                .iter()
                .map(|event| event.timestamp.as_str())
                .max()
                .unwrap_or_default()
                .to_owned(),
            parent: None,
            links: Vec::new(),
        };
        let mut links = HashMap::new();
        for (_, record) in records {
            if record.title.is_some() {
                meta.title.clone_from(&record.title);
            }
            if record.cwd.is_some() {
                meta.cwd.clone_from(&record.cwd);
            }
            if record.project.is_some() {
                meta.project.clone_from(&record.project);
            }
            if let Some(created) = &record.created {
                meta.created.clone_from(created);
            }
            if record.parent.is_some() {
                meta.parent.clone_from(&record.parent);
            }
            if let Some(link) = &record.link {
                links.insert(
                    (link.host.clone(), link.agent, link.native_id.clone()),
                    link.clone(),
                );
            }
        }
        meta.links = links.into_values().filter(|link| !link.removed).collect();
        meta.links.sort_by(|a, b| {
            (&a.host, a.agent.name(), &a.native_id).cmp(&(&b.host, b.agent.name(), &b.native_id))
        });
        meta
    }

    pub(crate) fn create(
        &self,
        meta: &SessionMeta,
        events: &[Event],
        created: Option<&str>,
    ) -> Result<()> {
        self.append(&meta.id, events)?;
        let mut links = meta.links.iter();
        self.record(
            &meta.id,
            &Record {
                title: meta.title.clone(),
                cwd: meta.cwd.clone(),
                project: meta.project.clone(),
                created: created.map(str::to_owned),
                parent: meta.parent.clone(),
                link: links.next().cloned(),
            },
        )?;
        for link in links {
            self.record(
                &meta.id,
                &Record {
                    link: Some(link.clone()),
                    ..Record::default()
                },
            )?;
        }
        Ok(())
    }

    pub(crate) fn append(&self, id: &str, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::new();
        for event in events {
            serde_json::to_writer(&mut lines, event)?;
            lines.push(b'\n');
        }
        let compressed = zstd::bulk::compress(&lines, 1)?;

        self.write_file(id, "events", "jsonl.zst", &compressed)
    }

    pub(crate) fn record(&self, id: &str, record: &Record) -> Result<()> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        self.write_file(id, "records", "json", &bytes)
    }

    fn write_file(&self, id: &str, kind: &str, extension: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.root.join("sessions").join(id).join(kind);
        fs::create_dir_all(&dir)?;
        let name = format!("{}-{}.{}", self.host, Uuid::now_v7(), extension);
        let path = dir.join(&name);
        let temporary = dir.join(format!(".{name}.tmp"));
        fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("publishing {}", path.display()))
    }
}

pub(crate) fn record_order(name: &str) -> (&str, &str) {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(name);
    let uuid = stem.get(stem.len().saturating_sub(36)..).unwrap_or(stem);
    (uuid, name)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().expect("store paths end in a file name");
    let temporary = path.with_file_name(format!(
        ".{}-{}.tmp",
        name.to_string_lossy(),
        Uuid::now_v7()
    ));
    fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    let result = fs::hard_link(&temporary, path);
    fs::remove_file(&temporary)?;
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("publishing {}", path.display())),
    }
}
