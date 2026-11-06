use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sync::session_id;
use crate::{Direction, Event, ForkPoint, SessionMeta, Store, journal};

// The sessions still in the old layout, `sessions/<id>/meta.json` next to `events.jsonl`
pub(super) fn old_sessions(data: &Path) -> Result<Vec<String>> {
    let dir = data.join("sessions");
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

// Forks are converted before the sessions they fork off, so a rerun after an interruption still finds every old parent to remap
pub(super) fn run(data: &Path, store: &Store) -> Result<usize> {
    let mut metas = HashMap::new();
    for old in old_sessions(data)? {
        let path = old_dir(data, &old).join("meta.json");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let meta: SessionMeta =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        metas.insert(old, meta);
    }
    let mut ids = HashMap::new();
    for old in metas.keys() {
        new_id(old, &metas, &mut ids);
    }
    let mut order: Vec<&String> = metas.keys().collect();
    order.sort_by_cached_key(|old| Reverse(depth(old, &metas)));
    for old in &order {
        let meta = &metas[*old];
        let migrated = SessionMeta {
            id: ids[*old].clone(),
            parent: meta.parent.as_ref().map(|parent| ForkPoint {
                session: ids
                    .get(&parent.session)
                    .cloned()
                    .unwrap_or_else(|| parent.session.clone()),
                event: parent.event.clone(),
            }),
            links: meta
                .links
                .iter()
                .cloned()
                .map(|mut link| {
                    link.host = store.host().to_owned();
                    link
                })
                .collect(),
            ..meta.clone()
        };
        let events = old_events(data, old)?;
        if !written(store, &migrated, &events)? {
            store.create(&migrated, &events, Some(&meta.created))?;
        }
        let dir = old_dir(data, old);
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(order.len())
}

fn old_dir(data: &Path, old: &str) -> PathBuf {
    data.join("sessions").join(old)
}

fn old_events(data: &Path, old: &str) -> Result<Vec<Event>> {
    let path = old_dir(data, old).join("events.jsonl");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    // Like the old store, a damaged line is left out instead of failing the session
    Ok(journal::lines(&bytes)
        .filter_map(|line| serde_json::from_slice(line.bytes).ok())
        .collect())
}

// Whether an earlier run already wrote the session completely and only stopped before removing the old one
fn written(store: &Store, meta: &SessionMeta, events: &[Event]) -> Result<bool> {
    if store.files(&meta.id, "records")?.is_empty() {
        return Ok(false);
    }
    let session = store.load(&meta.id)?;
    Ok(store.record_meta(&meta.id)?.created == meta.created
        && events.iter().all(|event| session.contains(&event.id))
        && meta.links.iter().all(|link| {
            session
                .meta
                .links
                .iter()
                .any(|stored| stored.is_for(&link.host, link.agent, &link.native_id))
        }))
}

// The deterministic id the session would have been created with, or one derived from its old id when no link tells
fn new_id(old: &str, metas: &HashMap<String, SessionMeta>, ids: &mut HashMap<String, String>) {
    if ids.contains_key(old) {
        return;
    }
    let meta = &metas[old];
    let id = match &meta.parent {
        Some(parent) => meta
            .links
            .iter()
            .find(|link| !link.native_id.is_empty())
            .map(|link| {
                let from = if metas.contains_key(&parent.session) {
                    new_id(&parent.session, metas, ids);
                    &ids[&parent.session]
                } else {
                    &parent.session
                };
                session_id::forked(from, parent.event.as_deref(), link.agent, &link.native_id)
            }),
        None => meta
            .links
            .iter()
            .find(|link| link.direction == Direction::Import && !link.native_id.is_empty())
            .map(|link| session_id::imported(link.agent, &link.native_id)),
    };
    ids.insert(
        old.to_owned(),
        id.unwrap_or_else(|| session_id::legacy(old)),
    );
}

fn depth(old: &str, metas: &HashMap<String, SessionMeta>) -> usize {
    match metas[old].parent.as_ref() {
        Some(parent) if metas.contains_key(&parent.session) => 1 + depth(&parent.session, metas),
        _ => 0,
    }
}
