//! Native session publication and removal preserve existing data on failure

use crate::support;

use std::fs;
use std::path::Path;

use baton_adapters::{CodexAdapter, CopilotAdapter, CursorAdapter, GrokAdapter, OpenhandsAdapter};
use baton_core::{Adapter, Agent, Blobs, Event, Export, Source, replay};

fn adapter(name: &str, root: &Path) -> Box<dyn Adapter> {
    match name {
        "copilot" => Box::new(CopilotAdapter::new(root.to_path_buf())),
        "cursor" => Box::new(CursorAdapter::new(root.to_path_buf())),
        "grok" => Box::new(GrokAdapter::new(root.to_path_buf())),
        "codex" => Box::new(CodexAdapter::new(root.to_path_buf())),
        "openhands" => Box::new(OpenhandsAdapter::new(root.to_path_buf())),
        _ => unreachable!(),
    }
}

fn with_export(f: impl FnOnce(&Export<'_>, &Blobs)) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::new(dir.path().join("blobs"));
    let events = Event::chain(
        [support::draft(support::user("hello", vec![]))],
        None,
        Agent::Pi,
        "source",
    );
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let export = Export {
        title: Some("A title"),
        cwd: Path::new("/work/demo"),
        created: "2026-01-01T00:00:00.000Z",
        events: &replayed,
        seed: Some(uuid::Uuid::parse_str("12345678-1234-5678-9234-567812345678").unwrap()),
    };
    f(&export, &blobs);
}

#[test]
fn codex_removes_titles_without_altering_other_index_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let adapter = CodexAdapter::new(root.clone());
    with_export(|export, blobs| {
        let session = adapter.write(export, blobs).unwrap();
        let index = root.join("session_index.jsonl");
        let original = fs::read_to_string(&index).unwrap();
        let other = "{\"id\":\"other\", \"thread_name\":\"first\"}\r\n\n{\"id\":\"other\",\"thread_name\":\"second\"}";
        fs::write(&index, format!("{original}{other}")).unwrap();
        adapter.remove(&session).unwrap();
        assert!(!session.path.exists());
        assert_eq!(fs::read_to_string(index).unwrap(), other);
    });
}

#[test]
fn codex_rejects_malformed_index_before_removing_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let adapter = CodexAdapter::new(root.clone());
    with_export(|export, blobs| {
        let session = adapter.write(export, blobs).unwrap();
        let index = root.join("session_index.jsonl");
        let original = fs::read_to_string(&index).unwrap() + "not json\n";
        fs::write(&index, &original).unwrap();
        assert!(adapter.remove(&session).is_err());
        assert!(session.path.is_file());
        assert_eq!(fs::read_to_string(index).unwrap(), original);
    });
}

#[test]
fn codex_removal_waits_for_native_index_writer() {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let adapter = CodexAdapter::new(root.clone());
    with_export(|export, blobs| {
        let session = adapter.write(export, blobs).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("session_index.jsonl.lock"))
            .unwrap();
        lock.lock().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let thread_root = root.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(CodexAdapter::new(thread_root).remove(&session))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let other = "{\"id\":\"other\",\"thread_name\":\"preserved\"}\n";
        let mut index = fs::OpenOptions::new()
            .append(true)
            .open(root.join("session_index.jsonl"))
            .unwrap();
        index.write_all(other.as_bytes()).unwrap();
        drop(lock);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("session_index.jsonl")).unwrap(),
            other
        );
    });
}

#[test]
fn codex_failed_title_append_cleans_rollout_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let adapter = CodexAdapter::new(root.clone());
    let index = root.join("session_index.jsonl");
    fs::create_dir_all(&index).unwrap();
    with_export(|export, blobs| {
        assert!(adapter.write(export, blobs).is_err());
        assert!(adapter.discover(None).unwrap().is_empty());
        fs::remove_dir(&index).unwrap();
        let session = adapter.write(export, blobs).unwrap();
        let found = adapter.discover(None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, session.id);
    });
}

#[test]
fn codex_preserves_existing_rollout_on_collision() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = CodexAdapter::new(dir.path().join("codex"));
    with_export(|export, blobs| {
        let session = adapter.write(export, blobs).unwrap();
        let original = fs::read(&session.path).unwrap();
        assert!(adapter.write(export, blobs).is_err());
        assert_eq!(fs::read(session.path).unwrap(), original);
    });
}

#[cfg(unix)]
#[test]
fn directories_are_cleaned_after_write_failure_and_seed_can_be_retried() {
    if let Some(name) = std::env::var_os("BATON_PUBLICATION_FAILURE_AGENT") {
        let root = std::env::var_os("BATON_PUBLICATION_FAILURE_ROOT").unwrap();
        with_export(|export, blobs| {
            let adapter = adapter(name.to_str().unwrap(), Path::new(&root));
            assert!(adapter.write(export, blobs).is_err());
            assert!(adapter.discover(None).unwrap().is_empty());
        });
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    with_export(|export, blobs| {
        for name in ["copilot", "cursor", "grok", "openhands"] {
            let reference_root = dir.path().join("reference").join(name);
            let session = adapter(name, &reference_root).write(export, blobs).unwrap();
            let relative = session.path.strip_prefix(&reference_root).unwrap();
            let root = dir.path().join("target").join(name);
            let session_dir = root.join(if session.path.is_dir() {
                relative
            } else {
                relative.parent().unwrap()
            });
            fs::create_dir_all(&session_dir).unwrap();
            fs::write(session_dir.join("keep"), "existing").unwrap();
            assert!(adapter(name, &root).write(export, blobs).is_err());
            assert_eq!(
                fs::read_to_string(session_dir.join("keep")).unwrap(),
                "existing"
            );
            fs::remove_dir_all(&session_dir).unwrap();

            support::run_in_child(
                "publication::directories_are_cleaned_after_write_failure_and_seed_can_be_retried",
                Some("ulimit -S -f 0; trap '' XFSZ"),
                |command| {
                    command
                        .env("BATON_PUBLICATION_FAILURE_AGENT", name)
                        .env("BATON_PUBLICATION_FAILURE_ROOT", &root);
                },
            );
            assert!(
                !session_dir.exists(),
                "{name} left a session directory behind"
            );
            let written = adapter(name, &root).write(export, blobs).unwrap();
            assert_eq!(written.path, root.join(relative));
            assert_eq!(adapter(name, &root).discover(None).unwrap().len(), 1);
        }
    });
}
