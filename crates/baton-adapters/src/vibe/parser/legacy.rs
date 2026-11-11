use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use baton_core::{Blobs, Cursor, Info, NativeRead, journal, time};
use serde_json::Value;

use crate::support::json;
use crate::vibe::parser::MessageParser;

pub(in crate::vibe) fn read(
    messages: &Path,
    cursor: Option<&Cursor>,
    blobs: &Blobs,
) -> Result<NativeRead> {
    let dir = messages
        .parent()
        .expect("message logs live in a session folder");
    let meta = meta(dir).with_context(|| format!("reading {}", dir.join("meta.json").display()))?;
    let created = meta["start_time"].as_str().and_then(time::normalize);
    let saved = meta["end_time"].as_str().and_then(time::normalize);
    let started = created.clone().unwrap_or_else(time::now);
    // Messages have no timestamps, and lines read after a cursor came with the last save
    let mut read = journal::read(messages, cursor, |leaf| {
        let timestamp = match leaf {
            Some(_) => saved.clone().unwrap_or_else(|| started.clone()),
            None => started.clone(),
        };
        MessageParser::new(blobs, dir, leaf, timestamp)
    })?;
    read.info = Info {
        cwd: json::string(&meta["environment"]["working_directory"]),
        title: json::string(&meta["title"]),
        created,
    };
    Ok(read)
}

pub(in crate::vibe) fn meta(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(dir.join("meta.json")).ok()?).ok()
}
