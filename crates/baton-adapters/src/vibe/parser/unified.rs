use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use baton_core::journal::{self, Line, Parser};
use baton_core::{Blobs, Draft, Info, NativeRead, Progress, Stamp, time};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{MessageParser, SUMMARY_TAGS, legacy};
use crate::support::{files, identifiers, json};

const STORE_FORMAT: &str = "mistral.vibe.unified-session-store/v1";
// The newest revision of the store layout this reader knows, the one Vibe 3.1 writes
const STORE_FORMAT_MINOR: u64 = 10;

// Reads the published generation of a session. One Vibe imported from a legacy folder starts with the folder's own read while the folder still holds what Vibe imported, since the import drops what baton keeps beside Vibe's fields
pub(in crate::vibe) fn read(current: &Path, blobs: &Blobs) -> Result<NativeRead> {
    let dir = current
        .parent()
        .expect("`CURRENT` lies in its session folder");
    let generation = Generation::open(dir)?;
    generation.check_format()?;
    let state = generation.public_state()?;
    let mut messages = generation.messages()?;
    let (mut drafts, mut info) = match generation.imported_folder()? {
        Some((folder, imported)) => {
            // A compaction leaves the imported messages out of the context
            if compaction_head(&messages) == 0 {
                ensure!(
                    imported <= messages.len(),
                    "Vibe session {} holds fewer messages than the legacy session it was imported from",
                    generation.session_id
                );
                messages.drain(..imported);
            }
            let read = legacy::read(&folder.join("messages.jsonl"), None, blobs)?;
            (read.drafts, read.info)
        }
        None => (Vec::new(), Info::default()),
    };
    // Messages have no timestamps, and the ones not imported came with the published generation
    let saved = json::timestamp(&generation.manifest["created_at"], &time::now());
    let mut own = parse(
        legacy_lines(messages),
        MessageParser::new(blobs, dir, None, saved),
    )?;
    // What the session added after an import ran on the model its own metadata names
    if let Some(meta) = legacy::meta(dir) {
        legacy::prepend_model(&meta, &mut own);
    }
    drafts.extend(own);
    let session = &state["session"];
    info.cwd = json::string(&generation.runtime["session_metadata"]["cwd"]).or(info.cwd);
    // Vibe leaves an imported folder's title behind, so only a rename there replaces it
    if info.title.is_none() || session["titleSource"] == "manual" {
        info.title = json::string(&session["title"]);
    }
    info.created = info.created.or_else(|| {
        session["createdAt"]
            .as_i64()
            .and_then(time::from_unix_millis)
    });
    Ok(NativeRead {
        info,
        drafts,
        continued: false,
        progress: Progress::default(),
    })
}

// The generation a session's `CURRENT` pointer publishes
pub(in crate::vibe) struct Generation {
    pub(in crate::vibe) session_id: String,
    pub(in crate::vibe) session_dir: PathBuf,
    generation_dir: PathBuf,
    pointer: Value,
    manifest: Value,
    pub(in crate::vibe) runtime: Value,
}

impl Generation {
    pub(in crate::vibe) fn open(session_dir: &Path) -> Result<Generation> {
        let current = session_dir.join("CURRENT");
        let pointer: Value = serde_json::from_slice(
            &fs::read(&current).with_context(|| format!("reading {}", current.display()))?,
        )
        .with_context(|| format!("parsing {}", current.display()))?;
        let generation_dir = session_dir
            .join("generations")
            .join(pointer["generation"].as_str().unwrap_or_default());
        let manifest = document(
            &generation_dir.join("manifest.json"),
            &pointer["manifest_sha256"],
        )?;
        let runtime = recorded(&generation_dir, &manifest["runtime_state"])?;
        Ok(Generation {
            session_id: session_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            session_dir: session_dir.to_path_buf(),
            generation_dir,
            pointer,
            manifest,
            runtime,
        })
    }

    fn check_format(&self) -> Result<()> {
        let format = &self.pointer["store_format"];
        // Pointers written before the store numbered its revisions leave the revision out
        let minor = self.pointer["store_format_minor"].as_u64().unwrap_or(1);
        if *format != STORE_FORMAT || minor > STORE_FORMAT_MINOR {
            bail!(
                "Vibe session {} is stored as {format} revision {minor}, which needs a newer baton: this one reads {STORE_FORMAT} up to revision {STORE_FORMAT_MINOR}",
                self.session_id
            );
        }
        Ok(())
    }

    // The legacy session Vibe imported this one from
    fn legacy_source(&self) -> Option<&Value> {
        let source = &self.runtime["import_provenance"]["source"];
        (source["backend"] == "legacy").then_some(source)
    }

    pub(in crate::vibe) fn legacy_id(&self) -> Option<&str> {
        self.legacy_source()?["session_id"].as_str()
    }

    pub(in crate::vibe) fn import_order(&self) -> (Option<&str>, &str) {
        (
            self.runtime["import_provenance"]["imported_at"].as_str(),
            self.session_id.as_str(),
        )
    }

    // The legacy folder Vibe imported the session from, and how many of its messages it imported, while the folder still holds what Vibe imported. A rewind replaces the context in place, after which it need not start with those messages
    fn imported_folder(&self) -> Result<Option<(PathBuf, usize)>> {
        let Some(source) = self.legacy_source() else {
            return Ok(None);
        };
        if !self.runtime["core_task_id"].is_null() {
            return Ok(None);
        }
        let root = self
            .session_dir
            .parent()
            .and_then(Path::parent)
            .expect("the Unified store lies in the session log directory");
        let id = source["session_id"].as_str().unwrap_or_default();
        // Vibe finds a legacy session by the first eight characters of its id, which end the folder's name
        let suffix = format!("_{}", id.get(..8).unwrap_or(id));
        for dir in files::read_dir(root)? {
            if !dir.to_string_lossy().ends_with(&suffix) {
                continue;
            }
            let (Ok(meta), Ok(messages)) = (
                fs::read(dir.join("meta.json")),
                fs::read(dir.join("messages.jsonl")),
            ) else {
                continue;
            };
            if source["store_revision"] == store_revision(&meta, &messages) {
                // Vibe imports every message but the system prompt
                let count = journal::lines(&messages)
                    .filter(|line| {
                        serde_json::from_slice::<Value>(line.bytes)
                            .is_ok_and(|message| message["role"] != "system")
                    })
                    .count();
                return Ok(Some((dir, count)));
            }
        }
        Ok(None)
    }

    fn journal(&self) -> PathBuf {
        self.session_dir.join(
            self.manifest["recovery_journal_segment"]["path"]
                .as_str()
                .unwrap_or_default(),
        )
    }

    // Vibe publishes a generation by replacing `CURRENT`, and journals what happens until the next one
    pub(in crate::vibe) fn stamp(&self) -> Result<Stamp> {
        let mut stamp = Stamp::of(&self.session_dir.join("CURRENT"))?;
        let journal = Stamp::of(&self.journal())?;
        stamp.size += journal.size;
        stamp.mtime = stamp.mtime.max(journal.mtime);
        Ok(stamp)
    }

    // The session's public state, as the recovery journal last set it or else as the generation published it. Vibe folds the journal into a new generation once a turn settles, and only changes to the session itself, such as an automatic title, stay behind
    fn public_state(&self) -> Result<Value> {
        let path = self.journal();
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut state = None;
        for line in journal::lines(&bytes) {
            let mut record: Value = serde_json::from_slice(line.bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            let changes = if record["type"] == "projection_delta" {
                record
                    .pointer_mut("/payload/delta")
                    .and_then(Value::as_array_mut)
            } else {
                None
            };
            let Some(changes) = changes
                .filter(|changes| changes.iter().all(|change| change["op"] == "set_envelope"))
            else {
                bail!(
                    "Vibe session {} has a turn it has not saved yet; let the turn finish in Vibe, then import again",
                    self.session_id
                );
            };
            if let Some(latest) = changes
                .last_mut()
                .and_then(|change| change.get_mut("state"))
            {
                state = Some(latest.take());
            }
        }
        match state {
            Some(state) => Ok(state),
            None => Ok(
                recorded(&self.generation_dir, &self.manifest["projection_state"])?
                    .get_mut("snapshot")
                    .map(Value::take)
                    .unwrap_or_default(),
            ),
        }
    }

    // The context messages of the checkpoint, which Vibe moves into chunks named by their digest when the manifest lists them
    fn messages(&self) -> Result<Vec<Value>> {
        let record = &self.manifest["checkpoint"];
        let Some(chunks) = record["chunks"].as_array() else {
            let mut checkpoint = recorded(&self.generation_dir, record)?;
            return match checkpoint.pointer_mut("/context/messages").map(Value::take) {
                Some(Value::Array(messages)) => Ok(messages),
                _ => bail!(
                    "the checkpoint of Vibe session {} holds no context messages",
                    self.session_id
                ),
            };
        };
        let mut messages = Vec::new();
        for chunk in chunks {
            let path = self
                .session_dir
                .join("chunks")
                .join(format!("{}.json", chunk.as_str().unwrap_or_default()));
            match document(&path, chunk)? {
                Value::Array(items) => messages.extend(items),
                _ => bail!("{} holds no context messages", path.display()),
            }
        }
        Ok(messages)
    }
}

// Vibe stores each document as JSON and a newline, and records the SHA-256 of the JSON
fn document(path: &Path, sha256: &Value) -> Result<Value> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if *sha256 != identifiers::sha256_hex(body) {
        bail!(
            "{} does not match the digest Vibe recorded for it",
            path.display()
        );
    }
    serde_json::from_slice(body).with_context(|| format!("parsing {}", path.display()))
}

// A document the manifest records by its file name in the generation folder and its digest
fn recorded(generation_dir: &Path, record: &Value) -> Result<Value> {
    document(
        &generation_dir.join(record["path"].as_str().unwrap_or_default()),
        &record["sha256"],
    )
}

// The revision Vibe records for the legacy folder it imports
fn store_revision(meta: &[u8], messages: &[u8]) -> String {
    let mut digest = Sha256::new();
    for part in [
        b"metadata\0".as_slice(),
        meta,
        b"\0messages\0",
        messages,
        b"\0",
    ] {
        digest.update(part);
    }
    base16ct::lower::encode_string(&digest.finalize())
}

fn parse(lines: Vec<Value>, mut parser: MessageParser<'_>) -> Result<Vec<Draft>> {
    let mut drafts = Vec::new();
    for (offset, line) in (0..).zip(lines) {
        if let Line::Node(node) = parser.line(offset, line)? {
            drafts.extend(node.drafts);
        }
    }
    Ok(drafts)
}

// The lines a legacy log holds for these context messages, so that one parser reads both
fn legacy_lines(mut messages: Vec<Value>) -> Vec<Value> {
    let rest = messages.split_off(compaction_head(&messages));
    let mut lines = Vec::with_capacity(rest.len() + 1);
    if !messages.is_empty() {
        let envelope: Vec<String> = messages.iter().map(text).collect();
        lines.push(json!({"role": "user", "content": envelope.join("\n"), "injected": true}));
    }
    lines.extend(rest.into_iter().map(legacy_line));
    lines
}

// How many messages open a compacted context: an injected preamble, the user messages it carries over and the summary closing them, which older Vibe versions logged as one message, then a reduced view of the messages after the summary that did not fit
fn compaction_head(messages: &[Value]) -> usize {
    let [opening, .., closing] = SUMMARY_TAGS;
    let injected = |message: &Value| message["source"] == "injection";
    let compacted = messages
        .first()
        .is_some_and(|first| injected(first) && text(first).contains(opening));
    if !compacted {
        return 0;
    }
    let Some(summary) = messages
        .iter()
        .position(|message| text(message).contains(closing))
    else {
        return 0;
    };
    summary + 1 + usize::from(messages.get(summary + 1).is_some_and(injected))
}

// The line a legacy log holds for a context message. A tool result's failure and images go where baton's own lines keep them, since legacy lines have no place for either
fn legacy_line(mut stored: Value) -> Value {
    let content = stored
        .pointer_mut("/message/content")
        .map(Value::take)
        .unwrap_or_default();
    let blocks = array(&content);
    let message = &stored["message"];
    let mut line = json!({"role": message["role"], "injected": stored["source"] == "injection"});
    match message["role"].as_str() {
        Some("user") => line["images"] = images(blocks),
        Some("assistant") => {
            let reasoning = blocks.iter().filter(|part| part["type"] == "reasoning");
            let thoughts: Vec<String> = reasoning
                .clone()
                .map(|part| json::texts(array(&part["content"]), &["text", "summary"]))
                .filter(|thought| !thought.is_empty())
                .collect();
            line["reasoning_content"] = json!(thoughts.join("\n"));
            let payloads: Vec<&Value> = reasoning
                .flat_map(|part| array(&part["_meta"]["vibe_reasoning_payloads"]))
                .collect();
            if !payloads.is_empty() {
                line["reasoning_payloads"] = json!(payloads);
            }
            line["tool_calls"] = blocks
                .iter()
                .filter(|part| part["type"] == "tool_call")
                .enumerate()
                .map(|(index, call)| json!({"id": call["id"], "index": index, "function": {"name": call["name"], "arguments": call["arguments"]["raw"]}, "type": "function"}))
                .collect();
        }
        Some("tool") => {
            line["name"] = message["name"].clone();
            line["tool_call_id"] = message["tool_call_id"].clone();
            line["tool_result"] = json!({"output": {"baton": {"is_error": message["outcome"] == "failure", "images": images(blocks)}}});
        }
        _ => {}
    }
    line["content"] = content;
    line
}

// Images the way legacy lines attach them, from image blocks and the image files Vibe embeds as resources
fn images(blocks: &[Value]) -> Value {
    blocks
        .iter()
        .filter_map(|block| {
            let image = match block["type"].as_str()? {
                "image" => block,
                "resource" => &block["resource"],
                _ => return None,
            };
            let mime = image["mimeType"]
                .as_str()
                .filter(|mime| mime.starts_with("image/"))?;
            let data = image["data"].as_str().or(image["blob"].as_str())?;
            Some(json!({"mime_type": mime, "data": data}))
        })
        .collect()
}

fn text(stored: &Value) -> String {
    json::texts(array(&stored["message"]["content"]), &["text"])
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}
