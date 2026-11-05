use std::collections::{HashMap, HashSet};
use std::mem;

use crate::Draft;
use crate::journal::Node;

#[derive(Default)]
pub(super) struct Tree {
    nodes: Vec<Node>,
    index: HashMap<String, usize>,
}

pub(super) struct ActivePath {
    pub(super) drafts: Vec<Draft>,
    pub(super) leaf: Option<String>,
}

impl Tree {
    pub(super) fn push(&mut self, node: Node) {
        if self.index.contains_key(&node.id) {
            return;
        }
        self.index.insert(node.id.clone(), self.nodes.len());
        self.nodes.push(node);
    }

    // Drafts along the ancestry of the last node, with anchored drafts moved before their anchor
    pub(super) fn active_path(mut self) -> ActivePath {
        let Some(last) = self.nodes.len().checked_sub(1) else {
            return ActivePath {
                drafts: Vec::new(),
                leaf: None,
            };
        };
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = Some(last);
        while let Some(index) = cursor.filter(|&index| seen.insert(index)) {
            chain.push(index);
            cursor = self.nodes[index]
                .parent
                .as_ref()
                .and_then(|parent| self.index.get(parent).copied());
        }
        chain.reverse();
        let mut positions: HashMap<&str, usize> = HashMap::new();
        for (position, &index) in chain.iter().enumerate() {
            positions.insert(self.nodes[index].id.as_str(), position);
        }
        let mut order: Vec<((usize, u8), usize)> = Vec::with_capacity(chain.len());
        for (here, &index) in chain.iter().enumerate() {
            let anchored = self.nodes[index]
                .anchor
                .as_deref()
                .and_then(|anchor| positions.get(anchor).copied())
                .filter(|&anchor| anchor < here);
            let key = anchored.map_or((here, 1), |anchor| (anchor, 0));
            order.push((key, index));
        }
        order.sort_by_key(|&(key, _)| key);
        let mut drafts = Vec::new();
        for (_, index) in order {
            drafts.append(&mut mem::take(&mut self.nodes[index].drafts));
        }
        ActivePath {
            drafts,
            leaf: Some(self.nodes[last].id.clone()),
        }
    }
}
