use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use baton_core::{Blobs, Stamp};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::{Map, Value};
use uuid::Uuid;
use zip::result::ZipResult;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::writer::Upload;
use crate::support::sqlite;

const DATABASE: &str = "rikka_hub.db";
// Older app versions packed the write-ahead log beside the database
const WAL: &str = "rikka_hub-wal";
const SHM: &str = "rikka_hub-shm";
const SETTINGS: &str = "settings.json";
// The built-in assistant, which the app adds back to settings that lack it
const DEFAULT_ASSISTANT: &str = "0950e2dc-9bd5-4801-afa3-aa887aa36b4e";
const SCHEMA: &str = include_str!("schema.sql");
// The schema version SCHEMA creates, the only one whose rows baton knows
const SCHEMA_VERSION: i64 = 32;

// A backup unpacked into a temporary directory, whose database writes edit in place until `pack` puts it back
pub(super) struct Unpacked {
    path: PathBuf,
    dir: tempfile::TempDir,
    // The assistant the app had selected, which lists the conversations written into the backup
    assistant: String,
    state: Mutex<State>,
}

struct State {
    // The entries the backup holds once packed
    entries: HashSet<String>,
    // Uploads added since the last pack
    added: Vec<String>,
    // Entries of the backup file that packing leaves out
    removed: HashSet<String>,
    changed: bool,
    // The backup file as it was unpacked or last packed, or none while it does not exist yet
    stamp: Option<Stamp>,
}

impl Unpacked {
    pub(super) fn open(path: &Path) -> Result<Unpacked> {
        let stamp = Stamp::of(path)?;
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive =
            ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;
        let Some(database) = archive.index_for_name(DATABASE) else {
            bail!(
                "{} holds no {DATABASE}; include the database when exporting the backup from RikkaHub",
                path.display()
            );
        };
        let dir = tempfile::tempdir()?;
        let db = dir.path().join(DATABASE);
        io::copy(&mut archive.by_index(database)?, &mut File::create(&db)?)?;
        if let Some(wal) = archive.index_for_name(WAL) {
            io::copy(
                &mut archive.by_index(wal)?,
                &mut File::create(dir.path().join(format!("{DATABASE}-wal")))?,
            )?;
        }
        let connection = sqlite::open_read_write(&db)?;
        // Writes then land in the database file itself, so packing that file alone keeps them
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |_| Ok(()))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            bail!(
                "{} holds a RikkaHub database of schema version {version}, while baton needs version {SCHEMA_VERSION} or newer",
                path.display()
            );
        }
        let assistant = match archive.index_for_name(SETTINGS) {
            Some(index) => {
                let settings: Map<String, Value> =
                    serde_json::from_reader(archive.by_index(index)?)
                        .with_context(|| format!("reading {SETTINGS} in {}", path.display()))?;
                match settings.get("assistantId") {
                    None => DEFAULT_ASSISTANT.to_owned(),
                    Some(Value::String(id)) if Uuid::parse_str(id).is_ok() => id.clone(),
                    Some(other) => bail!(
                        "{SETTINGS} in {} selects the assistant {other}, which is no assistant id",
                        path.display()
                    ),
                }
            }
            None => DEFAULT_ASSISTANT.to_owned(),
        };
        let entries: HashSet<String> = archive
            .file_names()
            .map(|name| name.map(Cow::into_owned))
            .collect::<ZipResult<_>>()?;

        Ok(Unpacked::staged(path, dir, assistant, entries, Some(stamp)))
    }

    // A new backup holding only what is written into it, without settings, so restoring it keeps the app's own
    pub(super) fn create(path: &Path) -> Result<Unpacked> {
        let dir = tempfile::tempdir()?;
        sqlite::open_or_create(&dir.path().join(DATABASE))?.execute_batch(SCHEMA)?;

        Ok(Unpacked::staged(
            path,
            dir,
            DEFAULT_ASSISTANT.to_owned(),
            HashSet::new(),
            None,
        ))
    }

    fn staged(
        path: &Path,
        dir: tempfile::TempDir,
        assistant: String,
        entries: HashSet<String>,
        stamp: Option<Stamp>,
    ) -> Unpacked {
        Unpacked {
            path: path.to_path_buf(),
            dir,
            assistant,
            state: Mutex::new(State {
                entries,
                added: Vec::new(),
                removed: HashSet::new(),
                changed: false,
                stamp,
            }),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn database(&self) -> PathBuf {
        self.dir.path().join(DATABASE)
    }

    pub(super) fn assistant(&self) -> &str {
        &self.assistant
    }

    // The bytes of the attachment `upload/<name>`, as added by this process or else packed in the backup
    pub(super) fn upload(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let staged = self
            .state
            .lock()
            .unwrap()
            .added
            .iter()
            .any(|added| added == name);
        if staged {
            return Ok(Some(fs::read(self.dir.path().join("upload").join(name))?));
        }
        let file =
            File::open(&self.path).with_context(|| format!("opening {}", self.path.display()))?;
        let mut archive = ZipArchive::new(file)?;
        let Some(index) = archive.index_for_name(&format!("upload/{name}")) else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        archive.by_index(index)?.read_to_end(&mut bytes)?;

        Ok(Some(bytes))
    }

    // Writers take turns, as they share the database and what is left to pack; uploads the backup already holds are not added twice
    pub(super) fn add(
        &self,
        uploads: &[Upload],
        blobs: &Blobs,
        rows: impl FnOnce(&Transaction) -> Result<()>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        fs::create_dir_all(self.dir.path().join("upload"))?;
        let mut added = Vec::new();
        for upload in uploads {
            let entry = format!("upload/{}", upload.name);
            if state.entries.contains(&entry) || added.contains(&upload.name) {
                continue;
            }
            fs::write(self.dir.path().join(&entry), blobs.get(&upload.blob)?)?;
            added.push(upload.name.clone());
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        rows(&transaction)?;
        transaction.commit()?;
        for name in added {
            state.entries.insert(format!("upload/{name}"));
            state.added.push(name);
        }
        state.changed = true;

        Ok(())
    }

    // Deletes a conversation the way the app does, then the uploads it referred to that no conversation left refers to; the other entries are not baton's to judge
    pub(super) fn remove(&self, conversation: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut referred = HashSet::new();
        {
            let mut statement = transaction
                .prepare("SELECT messages FROM message_node WHERE conversation_id = ?1")?;
            for messages in statement.query_map([conversation], |row| row.get::<_, String>(0))? {
                let messages: Value = serde_json::from_str(&messages?)?;
                for message in messages.as_array().into_iter().flatten() {
                    uploads(&message["parts"], &mut referred);
                }
            }
        }
        // Favorites keep no key to their conversation, so the app deletes those of its nodes along with it
        transaction.execute(
            "DELETE FROM favorites WHERE ref_key LIKE 'node:' || ?1 || ':%'",
            [conversation],
        )?;
        transaction.execute(
            "DELETE FROM ConversationEntity WHERE id = ?1",
            [conversation],
        )?;
        let mut unused = Vec::new();
        for name in referred {
            // How the app itself looks for a file in every branch of every conversation
            let quoted = serde_json::to_string(&format!("/files/upload/{name}"))?;
            let used: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM message_node WHERE instr(messages, ?1) > 0)",
                [&quoted[1..]],
                |row| row.get(0),
            )?;
            if !used {
                transaction.execute(
                    "DELETE FROM managed_files WHERE relative_path = ?1",
                    [format!("upload/{name}")],
                )?;
                unused.push(name);
            }
        }
        transaction.commit()?;
        for name in unused {
            let entry = format!("upload/{name}");
            state.added.retain(|added| *added != name);
            if state.entries.remove(&entry) {
                state.removed.insert(entry);
            }
        }
        state.changed = true;

        Ok(())
    }

    // Deleting a conversation then deletes its rows in the other tables, as the app's schema declares
    fn connection(&self) -> Result<Connection> {
        let connection = sqlite::open_read_write(&self.database())?;
        connection.pragma_update(None, "foreign_keys", true)?;

        Ok(connection)
    }

    // Replaces the backup with one holding the edited database and the added uploads, unless someone else replaced or created it since baton read it
    pub(super) fn pack(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.changed {
            return Ok(());
        }
        let folder = self.path.parent().expect("backup paths are resolved files");
        let target = tempfile::NamedTempFile::new_in(folder)?;
        let mut writer = ZipWriter::new(target.as_file());
        if state.stamp.is_some() {
            // Closed again before the backup is replaced, which Windows needs
            let mut archive = ZipArchive::new(File::open(&self.path)?)?;
            for index in 0..archive.len() {
                let entry = archive.by_index_raw(index)?;
                let replaced = {
                    let name = entry.name()?;
                    matches!(&*name, DATABASE | WAL | SHM) || state.removed.contains(&*name)
                };
                if !replaced {
                    writer.raw_copy_file(entry)?;
                }
            }
        }
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file(DATABASE, options)?;
        io::copy(&mut File::open(self.database())?, &mut writer)?;
        for name in &state.added {
            let entry = format!("upload/{name}");
            writer.start_file(&entry, options)?;
            io::copy(&mut File::open(self.dir.path().join(&entry))?, &mut writer)?;
        }
        writer.finish()?;
        let packed = Stamp::of(target.path())?;
        match state.stamp {
            Some(read) => {
                let current = Stamp::of(&self.path)?;
                if current != read {
                    bail!(
                        "{} changed after baton read it; run baton again to add to the backup as it is now",
                        self.path.display()
                    );
                }
                target
                    .persist(&self.path)
                    .with_context(|| format!("replacing {}", self.path.display()))?;
            }
            // A backup created meanwhile is kept, as persisting refuses to replace one
            None => {
                target
                    .persist_noclobber(&self.path)
                    .with_context(|| format!("creating {}", self.path.display()))?;
            }
        }
        state.added.clear();
        state.removed.clear();
        state.changed = false;
        state.stamp = Some(packed);

        Ok(())
    }
}

// The upload a URL points to, in the app's private upload folder on the phone
pub(super) fn upload_name(url: &str) -> Option<&str> {
    url.strip_prefix("file://")
        .and_then(|path| path.rsplit_once("/files/upload/"))
        .map(|(_, name)| name)
        .filter(|name| !name.contains('/'))
}

// The uploads parts refer to, including those inside tool results, the way the app collects the files of a conversation
fn uploads(parts: &Value, names: &mut HashSet<String>) {
    for part in parts.as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("image" | "document" | "video" | "audio") => {
                names.extend(
                    part["url"]
                        .as_str()
                        .and_then(upload_name)
                        .map(str::to_owned),
                );
            }
            Some("tool") => uploads(&part["output"], names),
            _ => {}
        }
    }
}
