use std::collections::HashMap;

use crate::{Event, SessionMeta};

/// A unified session loaded from the store.
pub struct Session {
    /// The contents of `meta.json`.
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
            self.index
                .entry(event.id.clone())
                .or_insert(self.events.len());
            self.events.push(event);
        }
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub(crate) fn last(&self) -> Option<&Event> {
        self.events.last()
    }

    pub(crate) fn leaf(&self) -> Option<&str> {
        self.last().map(|event| event.id.as_str())
    }

    /// The active branch, from the first event to the most recently appended one.
    pub fn path(&self) -> Vec<&Event> {
        self.leaf().map_or_else(Vec::new, |leaf| self.path_to(leaf))
    }

    pub(crate) fn path_to(&self, id: &str) -> Vec<&Event> {
        let mut path = Vec::new();
        let mut cursor = self.index.get(id).copied();
        while let Some(index) = cursor {
            let event = &self.events[index];
            path.push(event);
            // A parent is always appended before its children, which also rules out cycles in a damaged log
            cursor = event
                .parent
                .as_ref()
                .and_then(|parent| self.index.get(parent).copied())
                .filter(|&parent| parent < index);
        }
        path.reverse();
        path
    }
}
