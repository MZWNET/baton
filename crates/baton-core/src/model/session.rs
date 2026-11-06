use std::collections::{HashMap, HashSet};

use crate::{Event, SessionMeta};

/// A unified session loaded from the store.
pub struct Session {
    /// The session facts and links derived from its records.
    pub meta: SessionMeta,
    events: Vec<Event>,
    index: HashMap<String, usize>,
}

impl Session {
    pub(crate) fn new(meta: SessionMeta, events: Vec<Event>) -> Session {
        let mut session = Session {
            meta,
            events: Vec::new(),
            index: HashMap::new(),
        };
        session.extend(events);
        session
    }

    pub(crate) fn extend(&mut self, events: Vec<Event>) {
        for event in events {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.index.entry(event.id.clone())
            {
                entry.insert(self.events.len());
                self.events.push(event);
            }
        }
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub(crate) fn last(&self) -> Option<&Event> {
        self.leaves()
            .into_iter()
            .max_by(|a, b| (&a.timestamp, &a.id).cmp(&(&b.timestamp, &b.id)))
    }

    pub(crate) fn leaf(&self) -> Option<&str> {
        self.last().map(|event| event.id.as_str())
    }

    /// The active branch, ending at the latest leaf by timestamp and id.
    pub fn path(&self) -> Vec<&Event> {
        self.leaf().map_or_else(Vec::new, |leaf| self.path_to(leaf))
    }

    /// The paths to all leaves other than the active leaf, newest first.
    pub fn branches(&self) -> Vec<Vec<&Event>> {
        let mut leaves = self.leaves();
        leaves.sort_unstable_by(|a, b| (&b.timestamp, &b.id).cmp(&(&a.timestamp, &a.id)));
        leaves
            .into_iter()
            .skip(1)
            .map(|leaf| self.path_to(&leaf.id))
            .collect()
    }

    fn leaves(&self) -> Vec<&Event> {
        let parents: HashSet<&str> = self
            .events
            .iter()
            .filter_map(|event| event.parent.as_deref())
            .collect();

        self.events
            .iter()
            .filter(|event| !parents.contains(event.id.as_str()))
            .collect()
    }

    pub(crate) fn path_to(&self, id: &str) -> Vec<&Event> {
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = self.index.get(id).copied();
        while let Some(index) = cursor {
            if !visited.insert(index) {
                break;
            }
            let event = &self.events[index];
            path.push(event);
            cursor = event
                .parent
                .as_ref()
                .and_then(|parent| self.index.get(parent).copied());
        }
        path.reverse();
        path
    }
}
