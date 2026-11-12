use crate::{Body, Cursor, Draft, Event, Session, replay};

// What to do with the drafts of a native read
#[cfg_attr(test, derive(Debug, PartialEq))]
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

// The drafts continue `base`, past any thinking and meta events stored after it, such as a model `reimport` backfilled
pub(super) fn continuation(session: &Session, base: Option<String>, drafts: Vec<Draft>) -> Step {
    if drafts.is_empty() {
        return Step::Synced(base);
    }
    match session.leaf() {
        leaf if leaf == base.as_deref() => Step::Append { base, drafts },
        Some(leaf) if trails(session, leaf, base.as_deref()) => Step::Append {
            base: Some(leaf.to_owned()),
            drafts,
        },
        _ => Step::Fork { base, drafts },
    }
}

// The session was imported from this native session, so their events share ids, apart from thinking and meta events only one side has
pub(super) fn reimport(session: &Session, cursor: Option<&Cursor>, drafts: Vec<Draft>) -> Step {
    let mut position = None;
    let mut matched = 0;
    for (index, draft) in drafts.iter().enumerate() {
        match matching(session, position.as_deref(), &draft.body) {
            Some(id) => {
                position = Some(id);
                matched = index + 1;
            }
            // An older baton did not read it
            None if !draft.body.replayable() => {}
            None => break,
        }
    }
    let leaf = session.leaf();
    // A continuation must not branch off trailing thinking or meta events
    if let Some(leaf) = leaf
        && trails(session, leaf, position.as_deref())
    {
        position = Some(leaf.to_owned());
    }
    let backfill = position.as_deref().and_then(|position| {
        let (index, native) = drafts[..matched]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, draft)| replay::model(&draft.body).map(|model| (index, model)))?;
        let path = session.path_to(position);
        let stored = path
            .iter()
            .rev()
            .find_map(|event| replay::model(&event.body));
        (Some(native) != stored).then(|| (index, &path.last().unwrap().timestamp))
    });
    if matched == drafts.len() && (backfill.is_none() || leaf != position.as_deref()) {
        return Step::Synced(position);
    }
    let base = position;
    // Only this native session moved the leaf, so its own branch switches become branches here too
    let owned =
        leaf == base.as_deref() || cursor.is_some_and(|cursor| leaf == cursor.event.as_deref());
    let drafts: Vec<Draft> = drafts
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut draft)| match backfill {
            Some((model_index, timestamp)) if index == model_index => {
                // Keep the backfill after its base when leaves are ordered by timestamp
                draft.timestamp.clone_from(timestamp);
                Some(draft)
            }
            _ => (index >= matched).then_some(draft),
        })
        .collect();
    if owned {
        Step::Append { base, drafts }
    } else {
        Step::Fork { base, drafts }
    }
}

// The stored event with `body` that continues `position`, possibly past stored thinking and meta events
fn matching(session: &Session, position: Option<&str>, body: &Body) -> Option<String> {
    let id = Event::id_for(position, body);
    if session.contains(&id) {
        return Some(id);
    }
    session
        .children(position)
        .filter(|child| !child.body.replayable())
        .find_map(|child| matching(session, Some(&child.id), body))
}

// Whether `leaf` follows `position` past stored thinking and meta events only
fn trails(session: &Session, leaf: &str, position: Option<&str>) -> bool {
    session
        .children(position)
        .filter(|child| !child.body.replayable())
        .any(|child| child.id == leaf || trails(session, leaf, Some(&child.id)))
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
    continuation(session, base, drafts)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Agent, Assistant, SessionMeta, User};

    fn drafts(bodies: &[Body]) -> Vec<Draft> {
        bodies
            .iter()
            .map(|body| Draft {
                timestamp: "2026-01-01T00:00:00.000Z".into(),
                native_entry_id: None,
                body: body.clone(),
            })
            .collect()
    }

    fn user(text: &str) -> Body {
        Body::User(User {
            text: text.into(),
            images: vec![],
        })
    }

    fn assistant(text: &str) -> Body {
        Body::Assistant(Assistant { text: text.into() })
    }

    fn model() -> Body {
        Body::Meta(json!({"type": "model", "model": "m"}))
    }

    // A session imported with `bodies`, and the ids of its events
    fn stored(bodies: &[Body]) -> (Session, Vec<String>) {
        let mut read = drafts(bodies);
        for (index, draft) in read.iter_mut().enumerate() {
            draft.timestamp = format!("2026-01-01T00:00:{index:02}.000Z");
        }
        let events = Event::chain(read, None, Agent::Pi, "native");
        let ids = events.iter().map(|event| event.id.clone()).collect();
        let meta = SessionMeta {
            id: "session".into(),
            title: None,
            cwd: None,
            project: None,
            created: String::new(),
            updated: String::new(),
            parent: None,
            links: vec![],
        };
        (Session::new(meta, events), ids)
    }

    #[test]
    fn a_skipped_model_is_backfilled_after_the_last_match() {
        let (session, ids) = stored(&[user("u"), assistant("a")]);
        let read = drafts(&[user("u"), model(), assistant("a")]);
        let mut expected = drafts(&[model()]);
        expected[0].timestamp = "2026-01-01T00:00:01.000Z".into();
        assert_eq!(
            reimport(&session, None, read),
            Step::Append {
                base: Some(ids[1].clone()),
                drafts: expected,
            }
        );
    }

    #[test]
    fn a_model_already_stored_at_the_leaf_is_not_backfilled() {
        let (session, ids) = stored(&[
            user("u"),
            Body::Meta(json!({"type": "model", "model": "old"})),
            assistant("a"),
            model(),
        ]);
        let read = drafts(&[user("u"), model(), assistant("a")]);
        assert_eq!(
            reimport(&session, None, read),
            Step::Synced(Some(ids[3].clone()))
        );
    }

    #[test]
    fn a_stored_meta_the_drafts_lack_is_passed_over() {
        let (session, ids) = stored(&[user("u"), model(), assistant("a")]);
        let read = drafts(&[user("u"), assistant("a")]);
        assert_eq!(
            reimport(&session, None, read),
            Step::Synced(Some(ids[2].clone()))
        );
    }

    #[test]
    fn new_drafts_after_a_skipped_meta_continue_the_last_match() {
        let (session, ids) = stored(&[user("u"), assistant("a")]);
        let read = drafts(&[user("u"), model(), assistant("a"), user("u2")]);
        let mut expected = drafts(&[model(), user("u2")]);
        expected[0].timestamp = "2026-01-01T00:00:01.000Z".into();
        assert_eq!(
            reimport(&session, None, read),
            Step::Append {
                base: Some(ids[1].clone()),
                drafts: expected,
            }
        );
    }

    #[test]
    fn a_skipped_model_does_not_fork_an_older_native_branch() {
        let (session, ids) = stored(&[user("u"), assistant("a"), user("x")]);
        let read = drafts(&[user("u"), model(), assistant("a")]);
        assert_eq!(
            reimport(&session, None, read),
            Step::Synced(Some(ids[1].clone()))
        );
    }

    #[test]
    fn new_drafts_continue_a_trailing_stored_meta() {
        let (session, ids) = stored(&[user("u"), assistant("a"), model()]);
        let read = drafts(&[user("u"), assistant("a"), user("u2")]);
        assert_eq!(
            reimport(&session, None, read),
            Step::Append {
                base: Some(ids[2].clone()),
                drafts: drafts(&[user("u2")]),
            }
        );
    }

    #[test]
    fn diverging_drafts_fork_where_they_leave_the_session() {
        let (session, ids) = stored(&[user("u"), assistant("a"), user("u2"), assistant("a2")]);
        let read = drafts(&[user("u"), assistant("a"), user("u3"), assistant("a3")]);
        assert_eq!(
            reimport(&session, None, read),
            Step::Fork {
                base: Some(ids[1].clone()),
                drafts: drafts(&[user("u3"), assistant("a3")]),
            }
        );
    }

    #[test]
    fn an_incremental_read_continues_a_trailing_stored_meta() {
        let (session, ids) = stored(&[user("u"), assistant("a"), model()]);
        let read = drafts(&[user("u2")]);
        assert_eq!(
            continuation(&session, Some(ids[1].clone()), read),
            Step::Append {
                base: Some(ids[2].clone()),
                drafts: drafts(&[user("u2")]),
            }
        );
    }

    #[test]
    fn a_backflow_continues_a_trailing_stored_meta() {
        let (session, ids) = stored(&[user("u"), assistant("a"), model()]);
        let read = drafts(&[user("u"), assistant("a"), user("u2")]);
        assert_eq!(
            backflow(&session, 2, Some(ids[1].as_str()), read),
            Step::Append {
                base: Some(ids[2].clone()),
                drafts: drafts(&[user("u2")]),
            }
        );
    }

    #[test]
    fn an_incremental_read_forks_where_the_session_moved_on() {
        let (session, ids) = stored(&[user("u"), assistant("a"), user("u3")]);
        let read = drafts(&[user("u2")]);
        assert_eq!(
            continuation(&session, Some(ids[1].clone()), read),
            Step::Fork {
                base: Some(ids[1].clone()),
                drafts: drafts(&[user("u2")]),
            }
        );
    }
}
