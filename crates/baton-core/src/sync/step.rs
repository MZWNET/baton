use crate::{Cursor, Draft, Event, Session, replay};

// What to do with the drafts of a native read
pub(super) enum Step {
    // Nothing new; the native session ends at this event
    Synced(Option<String>),
    Append {
        base: Option<String>,
        drafts: Vec<Draft>,
    },
    Fork {
        base: Option<String>,
        drafts: Vec<Draft>,
    },
}

// The drafts of an incremental read continue the event the cursor recorded
pub(super) fn continuation(session: &Session, base: Option<String>, drafts: Vec<Draft>) -> Step {
    if drafts.is_empty() {
        Step::Synced(base)
    } else if session.leaf() == base.as_deref() {
        Step::Append { base, drafts }
    } else {
        Step::Fork { base, drafts }
    }
}

// The session was imported from this native session, so their events share ids
pub(super) fn reimport(session: &Session, cursor: Option<&Cursor>, drafts: Vec<Draft>) -> Step {
    let mut ids: Vec<String> = Vec::with_capacity(drafts.len());
    for draft in &drafts {
        let id = Event::id_for(ids.last().map(String::as_str), &draft.body);
        ids.push(id);
    }
    let Some(first) = ids.iter().position(|id| !session.contains(id)) else {
        return Step::Synced(ids.pop());
    };
    let base = first.checked_sub(1).map(|index| ids[index].clone());
    let leaf = session.leaf();
    // Only this native session moved the leaf, so its own branch switches become branches here too
    let owned = leaf == base.as_deref()
        || leaf.is_some_and(|leaf| ids.iter().any(|id| id == leaf))
        || cursor.is_some_and(|cursor| leaf == cursor.event.as_deref());
    let drafts: Vec<Draft> = drafts.into_iter().skip(first).collect();
    if owned {
        Step::Append { base, drafts }
    } else {
        Step::Fork { base, drafts }
    }
}

// The native session was exported from here, so its prefix only matches after replay normalization
pub(super) fn backflow(
    session: &Session,
    exported: usize,
    reached: Option<&str>,
    drafts: Vec<Draft>,
) -> Step {
    let path = reached.map_or_else(|| session.path(), |event| session.path_to(event));
    let ours = replay::events(&path);
    let Some(start) = align(&drafts, &ours, exported) else {
        // An emptied native session has nothing left to copy
        if drafts.is_empty() {
            return Step::Synced(None);
        }
        return Step::Fork { base: None, drafts };
    };
    let drafts: Vec<Draft> = drafts.into_iter().skip(start).collect();
    let base = if let Some(reached) = reached {
        Some(reached.to_owned())
    } else if ours.len() > exported {
        exported.checked_sub(1).map(|index| ours[index].id.clone())
    } else {
        session.leaf().map(String::from)
    };
    match (drafts.is_empty(), session.leaf() == base.as_deref()) {
        (true, _) => Step::Synced(base),
        (false, true) => Step::Append { base, drafts },
        (false, false) => Step::Fork { base, drafts },
    }
}

// Index just past the native drafts that replay the first `exported` events, when they still match
fn align(drafts: &[Draft], ours: &[&Event], exported: usize) -> Option<usize> {
    let start = drafts
        .iter()
        .rposition(|draft| draft.body.is_reset())
        .map_or(0, |index| index + 1);
    let theirs: Vec<usize> = (start..drafts.len())
        .filter(|&index| drafts[index].body.replayable())
        .take(exported)
        .collect();
    if theirs.len() < exported || ours.len() < exported {
        return None;
    }
    let matches = replay::digest(theirs.iter().map(|&index| &drafts[index].body))
        == replay::digest(ours[..exported].iter().map(|event| &event.body));
    matches.then(|| theirs.last().map_or(start, |&last| last + 1))
}
