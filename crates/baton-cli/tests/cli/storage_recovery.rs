//! Migration preserves legacy sessions when event files are incomplete or invalid

use std::fs;
use std::path::Path;

use super::support::Home;
use baton_core::{Agent, Body, Draft, Event, User};
use serde_json::json;

fn legacy(dir: &Path, id: &str, contents: Option<&str>) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("meta.json"),
        json!({
            "id": id, "title": "legacy", "cwd": null,
            "created": "2026-01-01T00:00:00.000Z", "updated": "2026-01-01T00:00:01.000Z",
            "links": []
        })
        .to_string(),
    )
    .unwrap();
    if let Some(contents) = contents {
        fs::write(dir.join("events.jsonl"), contents).unwrap();
    }
}

fn event() -> Event {
    Event::chain(
        [Draft {
            timestamp: "2026-01-01T00:00:01.000Z".into(),
            native_entry_id: None,
            body: Body::User(User {
                text: "retained event".into(),
                images: vec![],
            }),
        }],
        None,
        Agent::Pi,
        "native",
    )
    .remove(0)
}

#[test]
fn migration_rejects_corrupt_or_missing_events_without_removing_any_session() {
    let event = serde_json::to_string(&event()).unwrap();
    for contents in [Some(format!("{event}\n\ninvalid")), None] {
        let home = Home::new();
        let good = home.session("good");
        let bad = home.session("bad");
        legacy(&good, "good", Some(&event));
        legacy(&bad, "bad", contents.as_deref());
        let good_meta = fs::read(good.join("meta.json")).unwrap();
        let bad_meta = fs::read(bad.join("meta.json")).unwrap();

        let output = home.run(&["migrate"]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("events.jsonl"), "{stderr}");
        if contents.is_some() {
            assert!(stderr.contains("line 3"), "{stderr}");
        }
        assert_eq!(fs::read(good.join("meta.json")).unwrap(), good_meta);
        assert_eq!(
            fs::read(good.join("events.jsonl")).unwrap(),
            event.as_bytes()
        );
        assert_eq!(fs::read(bad.join("meta.json")).unwrap(), bad_meta);
        if let Some(contents) = contents {
            assert_eq!(
                fs::read(bad.join("events.jsonl")).unwrap(),
                contents.as_bytes()
            );
        } else {
            assert!(!bad.join("events.jsonl").exists());
        }
    }
}

#[test]
fn migration_preserves_unterminated_final_event_and_is_idempotent() {
    let home = Home::new();
    let old = home.session("old");
    legacy(&old, "old", Some(&serde_json::to_string(&event()).unwrap()));

    let first = home.run(&["migrate"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!old.exists());
    let listing = home.run(&["list"]);
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let listing = String::from_utf8(listing.stdout).unwrap();
    let ids: Vec<_> = listing
        .lines()
        .map(|line| line.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(ids.len(), 1);
    let show = home.run(&["show", ids[0]]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert!(
        String::from_utf8(show.stdout)
            .unwrap()
            .contains("retained event")
    );
    let files = |kind| {
        let mut names: Vec<_> = fs::read_dir(home.session(ids[0]).join(kind))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = (files("events"), files("records"));

    let second = home.run(&["migrate"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!((files("events"), files("records")), before);
    let after = home.run(&["list"]);
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(String::from_utf8(after.stdout).unwrap(), listing);
}
