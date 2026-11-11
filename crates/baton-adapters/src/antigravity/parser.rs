use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use baton_core::{
    Blobs, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, portable_call_id, time,
};
use rusqlite::Connection;
use serde_json::Value;
use uuid::Uuid;

use crate::support::normalize::{self, Content};
use crate::support::protobuf::Message;
use crate::support::{files, images, uri};

pub(super) fn databases<'a>(
    root: &'a Path,
    path: Option<&'a Path>,
) -> Result<(&'a Path, Vec<(PathBuf, String)>)> {
    let (root, dbs) = match path {
        Some(db) if db.is_file() => (
            db.parent().and_then(Path::parent).unwrap_or(db),
            vec![db.to_path_buf()],
        ),
        _ => {
            let root = path.unwrap_or(root);
            (root, files::read_dir(&root.join("conversations"))?)
        }
    };
    let dbs = dbs
        .into_iter()
        .filter_map(|db| {
            let stem = db.file_stem()?.to_str()?;
            let named = db.extension().is_some_and(|extension| extension == "db")
                && Uuid::parse_str(stem).is_ok();
            let id = named.then(|| stem.to_owned())?;
            Some((db, id))
        })
        .collect();
    Ok((root, dbs))
}

// Step types of the Cortex trajectory a conversation database stores
pub(super) const USER_INPUT: u64 = 14;
pub(super) const PLANNER_RESPONSE: u64 = 15;
pub(super) const GENERIC: u64 = 132;
const MCP_TOOL: u64 = 38;
// Built-in tools that newer stores keep as typed steps instead of wrapping them in a generic one
const TOOL_STEPS: [u64; 8] = [7, 8, 9, 21, 25, 86, 112, 128];
pub(super) const DONE: u64 = 3;
pub(super) const ERROR: u64 = 7;
// Legacy images and the flag of a compaction summary
pub(super) const BATON_FIELD: u32 = 47728;

// What a conversation database says about its session, besides its steps
pub(super) struct Conversation {
    pub(super) drafts: Vec<Draft>,
    pub(super) created: Option<String>,
    pub(super) cwd: Option<String>,
}

pub(super) fn read(connection: &Connection, blobs: &Blobs) -> Result<Conversation> {
    let blob: Vec<u8> = connection.query_row(
        "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
        [],
        |row| row.get(0),
    )?;
    let metadata = Message::parse(&blob)?;
    let created = metadata.message(2).and_then(|time| timestamp(&time));
    let cwd = metadata
        .message(1)
        .and_then(|workspace| workspace.string(1).map(String::from))
        .or_else(|| metadata.string(7).map(String::from))
        .and_then(|link| uri::path(&link));
    let mut reader = StepReader {
        blobs,
        pending: VecDeque::new(),
        emitted: HashSet::new(),
        timestamp: created.clone().unwrap_or_else(time::now),
        drafts: Vec::new(),
    };
    let mut statement =
        connection.prepare("SELECT idx, status, metadata, step_payload FROM steps ORDER BY idx")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let index: i64 = row.get(0)?;
        let status: u64 = row.get::<_, i64>(1)? as u64;
        let column: Option<Vec<u8>> = row.get(2)?;
        let payload: Vec<u8> = row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default();
        // A step that does not decode is one baton does not know, and is skipped like one
        if let Ok(step) = Message::parse(&payload) {
            reader.step(index, status, column.as_deref(), &step)?;
        }
    }
    Ok(Conversation {
        drafts: reader.drafts,
        created,
        cwd,
    })
}

// A trajectory stores times as protobuf timestamps: seconds and nanoseconds
pub(super) fn timestamp(message: &Message) -> Option<String> {
    let seconds = message.varint(1)? as i64;
    let nanos = message.varint(2).unwrap_or_default() as i64;
    time::from_unix_millis(seconds * 1000 + nanos / 1_000_000)
}

struct StepReader<'a> {
    blobs: &'a Blobs,
    // Calls without a result yet, which results that do not name their call answer in order
    pending: VecDeque<String>,
    emitted: HashSet<String>,
    // The time of the previous step, for steps that do not record one
    timestamp: String,
    drafts: Vec<Draft>,
}

impl StepReader<'_> {
    fn step(
        &mut self,
        index: i64,
        status: u64,
        column: Option<&[u8]>,
        step: &Message,
    ) -> Result<()> {
        let metadata = step
            .message(5)
            .or_else(|| column.and_then(|bytes| Message::parse(bytes).ok()));
        if let Some(time) = metadata
            .as_ref()
            .and_then(|metadata| metadata.message(1))
            .and_then(|time| timestamp(&time))
        {
            self.timestamp = time;
        }
        let baton = step.message(BATON_FIELD);
        let answered = metadata.as_ref().and_then(|metadata| metadata.message(4));
        let bodies = match step.varint(1).unwrap_or_default() {
            USER_INPUT => self.user(step.message(19), baton.as_ref())?,
            PLANNER_RESPONSE => step
                .message(20)
                .map(|planner| self.planner(&planner))
                .unwrap_or_default(),
            GENERIC => {
                let result = step.message(140).and_then(|generic| generic.message(2));
                let named = result.as_ref().and_then(linked_call);
                let call = answered
                    .and_then(|call| call.string(1).filter(|id| !id.is_empty()).map(String::from))
                    .or(named);
                let text = result
                    .as_ref()
                    .and_then(|result| result.string(1))
                    .unwrap_or_default();
                let mut attached = Vec::new();
                if let Some(result) = &result {
                    for media in result.repeated(4) {
                        attached.extend(self.media(&Message::parse(media)?)?);
                    }
                }
                self.merge_baton_images(&mut attached, baton.as_ref())?;
                self.result(call, text.to_owned(), attached, status)
            }
            MCP_TOOL => {
                let Some(mcp) = step.message(47) else {
                    return Ok(());
                };
                let mut bodies = Vec::new();
                let call = mcp.message(2);
                let id = call
                    .as_ref()
                    .and_then(|call| call.string(1))
                    .filter(|id| !id.is_empty())
                    .map(String::from);
                if let Some(call) = &call
                    && id.as_ref().is_some_and(|id| !self.emitted.contains(id))
                {
                    bodies.extend(self.call(call));
                }
                let text = mcp.string(3).or_else(|| mcp.string(8)).unwrap_or_default();
                let mut attached = Vec::new();
                for image in mcp.repeated(5) {
                    attached.extend(self.image_data(&Message::parse(image)?)?);
                }
                for media in mcp.repeated(6) {
                    attached.extend(self.media(&Message::parse(media)?)?);
                }
                self.merge_baton_images(&mut attached, baton.as_ref())?;
                bodies.extend(self.result(id, text.to_owned(), attached, status));
                bodies
            }
            kind if TOOL_STEPS.contains(&kind) || answered.is_some() => {
                let call = answered
                    .and_then(|call| call.string(1).filter(|id| !id.is_empty()).map(String::from));
                let (text, attached) = typed_output(kind, step, self)?;
                self.result(call, text, attached, status)
            }
            _ => Vec::new(),
        };
        for body in bodies {
            self.drafts.push(Draft {
                timestamp: self.timestamp.clone(),
                native_entry_id: Some(index.to_string()),
                body,
            });
        }
        Ok(())
    }

    // baton marks the summaries it writes as user input
    fn user(&self, input: Option<Message>, baton: Option<&Message>) -> Result<Vec<Body>> {
        let Some(input) = input else {
            return Ok(Vec::new());
        };
        let text = [input.string(2), input.string(1)]
            .into_iter()
            .flatten()
            .find(|text| !text.is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                input
                    .repeated(3)
                    .filter_map(|item| Message::parse(item).ok()?.string(1).map(String::from))
                    .collect()
            });
        if baton.is_some_and(|baton| baton.varint(2) == Some(1)) {
            return Ok(vec![Body::Compaction(Compaction { summary: text })]);
        }
        let mut attached = Vec::new();
        for image in input.repeated(5) {
            attached.extend(self.image_data(&Message::parse(image)?)?);
        }
        self.merge_baton_images(&mut attached, baton)?;
        let content = Content {
            text,
            images: attached,
        };
        Ok(normalize::user(content).into_iter().collect())
    }

    fn planner(&mut self, planner: &Message) -> Vec<Body> {
        let mut bodies = Vec::new();
        let thoughtful = [3, 4, 14, 16]
            .iter()
            .any(|&field| planner.bytes(field).is_some())
            || planner.varint(5) == Some(1);
        if thoughtful {
            let signature = planner
                .message(14)
                .and_then(|signature| signature.message(2))
                .and_then(|signature| signature.bytes(1).map(images::encode));
            bodies.push(Body::Thinking(Thinking {
                text: planner.string(3).unwrap_or_default().to_owned(),
                signature,
            }));
        }
        let text = planner
            .string(1)
            .filter(|text| !text.is_empty())
            .or_else(|| planner.string(8));
        bodies.extend(text.and_then(normalize::assistant));
        for call in planner.repeated(7) {
            if let Ok(call) = Message::parse(call) {
                bodies.extend(self.call(&call));
            }
        }
        bodies
    }

    fn call(&mut self, call: &Message) -> Option<Body> {
        let id = call.string(1).filter(|id| !id.is_empty())?.to_owned();
        let input = match call.string(3).filter(|arguments| !arguments.is_empty()) {
            Some(arguments) => normalize::arguments(&Value::from(arguments)),
            None => serde_json::json!({}),
        };
        self.pending.push_back(id.clone());
        self.emitted.insert(id.clone());
        Some(Body::ToolCall(ToolCall {
            call_id: portable_call_id(&id),
            name: call.string(2).unwrap_or("unknown").to_owned(),
            input,
        }))
    }

    // A result that does not say which call it answers answers the oldest one still waiting
    fn result(
        &mut self,
        call: Option<String>,
        text: String,
        images: Vec<Image>,
        status: u64,
    ) -> Vec<Body> {
        let call = match call {
            Some(call) => {
                self.pending.retain(|pending| *pending != call);
                call
            }
            None => match self.pending.pop_front() {
                Some(call) => call,
                None => return Vec::new(),
            },
        };
        vec![Body::ToolResult(ToolResult {
            call_id: portable_call_id(&call),
            text,
            images,
            is_error: status == ERROR,
        })]
    }

    fn baton_images(&self, baton: Option<&Message>) -> Result<Vec<Image>> {
        let mut found = Vec::new();
        for image in baton.into_iter().flat_map(|baton| baton.repeated(1)) {
            let image = Message::parse(image)?;
            if let (Some(mime), Some(bytes)) = (image.string(1), image.bytes(2)) {
                found.push(images::store(self.blobs, mime, bytes)?);
            }
        }
        Ok(found)
    }

    fn merge_baton_images(&self, attached: &mut Vec<Image>, baton: Option<&Message>) -> Result<()> {
        let mut matched = vec![false; attached.len()];
        for image in self.baton_images(baton)? {
            if let Some(index) = attached
                .iter()
                .zip(&matched)
                .position(|(native, matched)| !matched && *native == image)
            {
                matched[index] = true;
            } else {
                attached.push(image);
            }
        }
        Ok(())
    }

    fn image_data(&self, image: &Message) -> Result<Option<Image>> {
        let Some(mime) = image.string(2).filter(|mime| mime.starts_with("image/")) else {
            return Ok(None);
        };
        if let Some(data) = image.string(1).filter(|data| !data.is_empty()) {
            let bytes =
                images::decode(data).ok_or_else(|| anyhow!("invalid Antigravity image base64"))?;
            return images::store(self.blobs, mime, &bytes).map(Some);
        }
        match image.string(4) {
            Some(uri) => self.image_uri(mime, uri),
            None => Ok(None),
        }
    }

    fn media(&self, media: &Message) -> Result<Option<Image>> {
        let Some(mime) = media.string(1).filter(|mime| mime.starts_with("image/")) else {
            return Ok(None);
        };
        if let Some(bytes) = media.bytes(2) {
            return images::store(self.blobs, mime, bytes).map(Some);
        }
        match media.string(5) {
            Some(uri) => self.image_uri(mime, uri),
            None => Ok(None),
        }
    }

    fn image_uri(&self, mime: &str, value: &str) -> Result<Option<Image>> {
        if value.starts_with("data:") {
            return images::from_data_url(self.blobs, value);
        }
        let path = match uri::path(value) {
            Some(path) => path,
            None if !value.contains("://") => value.to_owned(),
            None => return Ok(None),
        };
        match fs::read(&path) {
            Ok(bytes) => images::store(self.blobs, mime, &bytes).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading image file {path}")),
        }
    }
}

// session-migrate links a generic result to its call in its own result entry
fn linked_call(result: &Message) -> Option<String> {
    result.repeated(2).find_map(|entry| {
        let entry = Message::parse(entry).ok()?;
        (entry.string(1)? == "session_migrate_call_id").then(|| {
            entry
                .string(2)
                .filter(|id| !id.is_empty())
                .map(String::from)
        })?
    })
}

// The output of a typed tool step, where its type keeps one
fn typed_output(kind: u64, step: &Message, reader: &StepReader) -> Result<(String, Vec<Image>)> {
    let text = match kind {
        8 => step
            .message(14)
            .and_then(|view| view.string(4).or_else(|| view.string(9)).map(String::from)),
        21 => step.message(28).and_then(|command| {
            [21, 26]
                .iter()
                .find_map(|&field| command.message(field)?.string(1).map(String::from))
                .or_else(|| {
                    command
                        .string(4)
                        .or_else(|| command.string(5))
                        .map(String::from)
                })
        }),
        112 => step.message(127).and_then(|shell| {
            shell
                .string(3)
                .or_else(|| shell.string(4))
                .map(String::from)
        }),
        _ => None,
    };
    let mut attached = Vec::new();
    if kind == 8
        && let Some(media) = step.message(14).and_then(|view| view.message(15))
    {
        attached.extend(reader.media(&media)?);
    }
    Ok((text.unwrap_or_default(), attached))
}
