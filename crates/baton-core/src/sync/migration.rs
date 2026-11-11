use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::Cursors;
use crate::model::Record;
use crate::storage::replace;
use crate::sync::session_id;
use crate::{
    Agent, Body, Direction, Event, ForkPoint, Index, Migrated, SessionMeta, Store,
    portable_call_id, project,
};

const VERSION: u32 = 2;
const JOURNAL: &str = ".migration.tmp";

fn supported_version(data: &Path) -> Result<u32> {
    let path = data.join("version");
    let version = match fs::read_to_string(&path) {
        Ok(text) => text
            .trim()
            .parse::<u32>()
            .with_context(|| format!("parsing {}", path.display()))?,
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if version > VERSION {
        bail!("{} uses a newer data format; upgrade baton", data.display());
    }

    Ok(version)
}

pub(super) fn ensure_current_format(data: &Path) -> Result<()> {
    let version = supported_version(data)?;
    let sessions = data.join("sessions");
    let has_sessions = match fs::read_dir(&sessions) {
        Ok(mut entries) => entries.next().transpose()?.is_some(),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("reading {}", sessions.display())),
    };
    if data.join(JOURNAL).exists()
        || !old_sessions(data)?.is_empty()
        || (has_sessions && version < VERSION)
    {
        bail!(
            "{} uses an old data format; run `baton migrate`",
            data.display()
        );
    }
    if !has_sessions && version < VERSION {
        write_version(data)?;
    }

    Ok(())
}

fn write_version(data: &Path) -> Result<()> {
    replace(&data.join("version"), format!("{VERSION}\n").as_bytes())
}

fn migrated_call_id(id: &str) -> Option<String> {
    let (call_suffix, function) = id.strip_prefix("call_")?.split_once('_')?;
    let (prefix, suffix) = function.split_once('_')?;
    if call_suffix.is_empty()
        || !matches!(prefix, "fc" | "ctc")
        || suffix.is_empty()
        || !call_suffix.bytes().all(|c| c.is_ascii_alphanumeric())
        || !suffix.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }

    Some(portable_call_id(&format!("call_{call_suffix}|{function}")))
}

#[derive(Serialize, Deserialize)]
struct Journal {
    event_ids: HashMap<String, String>,
    rewritten_calls: HashMap<String, HashSet<String>>,
}

pub(super) fn migrate(data: &Path, cache: &Path, store: &Store) -> Result<Migrated> {
    supported_version(data)?;
    let converted = convert_layout(data, store)?;
    let journal = load_or_plan_journal(data, store)?;
    apply(store, &Cursors::open(cache, data)?, &journal)?;
    let rewritten = journal.rewritten_calls.len();
    let corrected = correct_projects(store)?;
    write_version(data)?;
    Index::remove(cache)?;
    let path = data.join(JOURNAL);
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;

    Ok(Migrated {
        converted,
        rewritten,
        corrected,
    })
}

fn load_or_plan_journal(data: &Path, store: &Store) -> Result<Journal> {
    let path = data.join(JOURNAL);
    match fs::read(&path) {
        Ok(bytes) => {
            return serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    let files = store.session_files()?;
    let mut metas = HashMap::new();
    for session in &files {
        metas.insert(session.id.clone(), store.record_meta(&session.id)?);
    }
    let mut order: Vec<_> = files.iter().collect();
    order.sort_by_cached_key(|session| depth(&session.id, &metas));
    let mut event_ids: HashMap<String, String> = HashMap::new();
    let mut rewritten_calls: HashMap<String, HashSet<String>> = HashMap::new();
    for session in order {
        let meta = &metas[&session.id];
        let exports: HashSet<_> = meta
            .links
            .iter()
            .filter(|link| link.direction == Direction::Export)
            .map(|link| (link.agent, link.native_id.as_str()))
            .collect();
        let mut events = Vec::new();
        for name in &session.events {
            events.extend(store.segment(&session.id, name)?);
        }
        let positions: HashMap<_, _> = events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.clone(), index))
            .collect();
        let mut done = vec![false; events.len()];
        let mut changed = vec![false; events.len()];
        for index in 0..events.len() {
            let mut pending = Vec::new();
            let mut current = Some(index);
            while let Some(next) = current {
                if done[next] {
                    break;
                }
                pending.push(next);
                current = events[next]
                    .parent
                    .as_ref()
                    .and_then(|parent| positions.get(parent).copied());
            }
            for next in pending.into_iter().rev() {
                let event = &mut events[next];
                let old = event.id.clone();
                if let Some(parent) = &mut event.parent
                    && let Some(mapped) = event_ids.get(parent)
                {
                    *parent = mapped.clone();
                    changed[next] = true;
                }
                if matches!(event.origin.agent, Agent::Pi | Agent::Omp)
                    && !exports.contains(&(event.origin.agent, event.origin.native_id.as_str()))
                {
                    let call_id = match &mut event.body {
                        Body::ToolCall(call) => Some(&mut call.call_id),
                        Body::ToolResult(result) => Some(&mut result.call_id),
                        _ => None,
                    };
                    if let Some(call_id) = call_id
                        && let Some(mapped) = migrated_call_id(call_id)
                    {
                        *call_id = mapped;
                        changed[next] = true;
                        rewritten_calls
                            .entry(session.id.clone())
                            .or_default()
                            .insert(old.clone());
                    }
                }
                if changed[next] {
                    event.id = Event::id_for(event.parent.as_deref(), &event.body);
                    event_ids.insert(old, event.id.clone());
                }
                done[next] = true;
            }
        }
    }

    let journal = Journal {
        event_ids,
        rewritten_calls,
    };
    replace(&path, &serde_json::to_vec(&journal)?)?;

    Ok(journal)
}

fn apply(store: &Store, cursors: &Cursors, journal: &Journal) -> Result<()> {
    let files = store.session_files()?;
    for session in &files {
        for (name, mut record) in store.read_records(&session.id, &session.records)? {
            let mut changed = false;
            for id in [
                record
                    .parent
                    .as_mut()
                    .and_then(|parent| parent.event.as_mut()),
                record
                    .link
                    .as_mut()
                    .and_then(|link| link.exported_leaf.as_mut()),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(mapped) = journal.event_ids.get(id) {
                    *id = mapped.clone();
                    changed = true;
                }
            }
            if changed {
                store.replace_record(&session.id, &name, &record)?;
            }
        }
    }
    cursors.remap_events(&journal.event_ids)?;
    for session in &files {
        for name in &session.events {
            let mut events = store.segment(&session.id, name)?;
            let mut changed = false;
            for event in &mut events {
                if let Some(mapped) = journal.event_ids.get(&event.id) {
                    if journal
                        .rewritten_calls
                        .get(&session.id)
                        .is_some_and(|calls| calls.contains(&event.id))
                    {
                        let call_id = match &mut event.body {
                            Body::ToolCall(call) => &mut call.call_id,
                            Body::ToolResult(result) => &mut result.call_id,
                            _ => unreachable!(),
                        };
                        *call_id = migrated_call_id(call_id).expect("journal records old call ids");
                    }
                    event.id.clone_from(mapped);
                    if let Some(parent) = &mut event.parent
                        && let Some(mapped) = journal.event_ids.get(parent)
                    {
                        parent.clone_from(mapped);
                    }
                    changed = true;
                }
            }
            if changed {
                store.replace_segment(&session.id, name, &events)?;
            }
        }
    }

    Ok(())
}

fn correct_projects(store: &Store) -> Result<usize> {
    let mut changed = 0;
    for session in store.session_files()? {
        let meta = store.record_meta(&session.id)?;
        let Some(mut identity) = meta.project else {
            continue;
        };
        let Some(remote) = identity.remote.as_deref() else {
            continue;
        };
        if remote.split('/').next() == Some("github.com") {
            continue;
        }
        let cwd = meta
            .cwd
            .as_deref()
            .and_then(|cwd| project::identify(Path::new(cwd)));
        let candidates: HashSet<_> = project::checkouts()
            .iter()
            .map(|(_, project)| project)
            .chain(cwd.iter())
            .filter_map(|project| project.remote.as_deref())
            .filter(|candidate| candidate.eq_ignore_ascii_case(remote))
            .collect();
        if candidates.len() == 1 {
            let candidate = *candidates.iter().next().unwrap();
            if candidate != remote {
                identity.remote = Some(candidate.to_owned());
                store.record(
                    &session.id,
                    &Record {
                        project: Some(identity),
                        ..Record::default()
                    },
                )?;
                changed += 1;
            }
        }
    }

    Ok(changed)
}

// The sessions still in the old layout, `sessions/<id>/meta.json` next to `events.jsonl`
fn old_sessions(data: &Path) -> Result<Vec<String>> {
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
fn convert_layout(data: &Path, store: &Store) -> Result<usize> {
    let mut metas = HashMap::new();
    for old in old_sessions(data)? {
        let path = old_dir(data, &old).join("meta.json");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let meta: SessionMeta =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        old_events(data, &old)?;
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
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    bytes
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(number, line)| {
            serde_json::from_slice(line)
                .with_context(|| format!("parsing {} line {}", path.display(), number + 1))
        })
        .collect()
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
