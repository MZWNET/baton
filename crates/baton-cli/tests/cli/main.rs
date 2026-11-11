//! End-to-end runs of the `baton` binary against isolated agent stores: incremental imports, backflow, forks and materialization

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
mod migration;
mod paseo_failures;
mod storage_recovery;
mod support;

use support::{Home, append, claude_session, isolate_git, jsonl};

fn paseo_fixture(home: &Home) -> (String, PathBuf) {
    let native_id = "11111111-1111-4111-8111-111111111111";
    let native = claude_session(home, native_id);
    let record = home.path(".paseo/agents/work/paseo-1.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(record, json!({"id": "paseo-1", "provider": "claude", "cwd": home.project(), "title": "Paseo title", "launchEnv": {"EXAMPLE": "private fixture value"}, "persistence": {"provider": "claude", "sessionId": native_id, "nativeHandle": native_id, "metadata": {"mcpServers": {"example": "private fixture value"}}}}).to_string()).unwrap();
    (native_id.to_owned(), native)
}

#[test]
fn paseo_import_fills_a_missing_title_and_is_idempotent() {
    let home = Home::new();
    paseo_fixture(&home);
    home.baton(&["import", "--agent", "claude"]);
    home.baton(&["import", "--agent", "paseo"]);
    let sessions = home.sessions(&[]);
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert!(
        home.baton(&["list", "--agent", "paseo"])
            .contains("Paseo title")
    );
    assert!(home.baton(&["show", "paseo:paseo-1"]).contains(session));
    let records = home.path(&format!("baton/sessions/{session}/records"));
    assert!(fs::read_dir(&records).unwrap().all(|entry| {
        !fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .contains("private fixture value")
    }));
    let before = fs::read_dir(&records).unwrap().count();
    home.baton(&["import", "--agent", "paseo"]);
    assert_eq!(fs::read_dir(records).unwrap().count(), before);
}

#[test]
fn paseo_import_preserves_the_provider_title_and_is_idempotent() {
    let home = Home::new();
    let (native_id, native) = paseo_fixture(&home);
    append(
        &native,
        &[json!({"type": "custom-title", "customTitle": "Provider title", "sessionId": native_id})],
    );
    home.baton(&["import", "--agent", "claude"]);
    let session = home.sessions(&[]).remove(0);
    assert!(
        home.baton(&["show", &session])
            .contains("title    Provider title")
    );
    home.baton(&["import", "--agent", "paseo"]);
    let shown = home.baton(&["show", "paseo:paseo-1"]);
    assert!(shown.contains("title    Provider title"));
    assert!(!shown.contains("title    Paseo title"));
    let records = home.path(&format!("baton/sessions/{session}/records"));
    let before = fs::read_dir(&records).unwrap().count();
    home.baton(&["import", "--agent", "paseo"]);
    assert_eq!(fs::read_dir(records).unwrap().count(), before);
    assert!(
        home.baton(&["show", &session])
            .contains("title    Provider title")
    );
}

#[test]
fn paseo_import_rescues_a_deleted_provider_session() {
    let home = Home::new();
    paseo_fixture(&home);
    home.baton(&["import", "--agent", "claude"]);
    let session = home.sessions(&[]).remove(0);
    let target = Home::new();
    copy_session_files(&home, &target, &session);
    let (_, native) = paseo_fixture(&target);
    fs::remove_file(native).unwrap();
    target.baton(&["import", "--agent", "paseo"]);
    assert!(target.baton(&["show", "paseo:paseo-1"]).contains(&session));
    assert!(
        target
            .baton(&["list", "--agent", "paseo"])
            .contains("Paseo title")
    );
}

#[test]
fn paseo_import_rescues_a_deleted_provider_by_another_agents_id() {
    let home = Home::new();
    let (native_id, native) = paseo_fixture(&home);
    fs::remove_file(native).unwrap();
    let mut session_header = header("/work");
    session_header["id"] = json!(native_id);
    write_pi(
        &home,
        &[session_header, ask("a1", None, 1, "saved conversation")],
    );
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);
    home.baton(&["import", "--agent", "paseo"]);
    assert_eq!(home.sessions(&[]), vec![session.clone()]);
    assert!(home.baton(&["show", "paseo:paseo-1"]).contains(&session));
    let records = home.path(&format!("baton/sessions/{session}/records"));
    let links: Vec<Value> = fs::read_dir(records)
        .unwrap()
        .filter_map(|entry| {
            let record: Value =
                serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
            (record["link"]["agent"] == "paseo").then(|| record["link"].clone())
        })
        .collect();
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0]["provider"],
        json!({"agent": "claude", "native_id": native_id})
    );
}

#[cfg(unix)]
#[test]
fn paseo_materialize_registers_once_and_handles_a_missing_daemon() {
    let source = Home::new();
    paseo_fixture(&source);
    source.baton(&["import", "--agent", "paseo"]);
    let session = source.sessions(&[]).remove(0);
    for running in [true, false] {
        let target = Home::new();
        copy_session_files(&source, &target, &session);
        write_pi(
            &target,
            &[header("/other"), ask("a1", None, 1, "unrelated session")],
        );
        target.baton(&["import", "--agent", "pi"]);
        target.install_paseo(running);
        let output = target.baton_output(&["materialize", "--in", "paseo"], &[]);
        assert!(output.status.success());
        let transcripts = target.claude_transcripts();
        assert_eq!(transcripts.len(), 1);
        let records = target.path(&format!("baton/sessions/{session}/records"));
        if running {
            let log = fs::read_to_string(target.path("paseo.log")).unwrap();
            let native_id = transcripts[0].file_stem().unwrap().to_str().unwrap();
            assert!(log.contains(&format!(
                "import {native_id} --provider claude --cwd {} --json",
                source.project().display()
            )));
            assert!(
                target
                    .baton(&["show", "paseo:restored-1"])
                    .contains(&session)
            );
            let before = fs::read_dir(&records).unwrap().count();
            target.baton(&["materialize", "--in", "paseo"]);
            assert_eq!(fs::read_dir(&records).unwrap().count(), before);
            let log = fs::read_to_string(target.path("paseo.log")).unwrap();
            assert_eq!(
                log.lines()
                    .filter(|line| line.starts_with("import "))
                    .count(),
                1
            );
        } else {
            assert!(!target.baton(&["list", "--agent", "paseo"]).is_empty());
            let host = format!(
                "test-{}",
                target.dir.path().file_name().unwrap().to_string_lossy()
            );
            assert!(!fs::read_dir(records).unwrap().any(|entry| {
                let record: Value =
                    serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
                record["link"]["agent"] == "paseo" && record["link"]["host"] == host
            }));
        }
    }
}

#[cfg(unix)]
#[test]
fn paseo_materialize_replaces_stale_untouched_exports() {
    let source = Home::new();
    let (native_id, native) = paseo_fixture(&source);
    source.baton(&["import", "--agent", "paseo"]);
    let session = source.sessions(&[]).remove(0);
    let target = Home::new();
    copy_session_files(&source, &target, &session);
    target.install_paseo(true);
    target.baton(&["materialize", "--in", "paseo"]);
    let old = target.claude_transcripts().remove(0);
    append(
        &native,
        &[
            json!({"type": "user", "uuid": "a2", "parentUuid": "a1", "sessionId": native_id, "cwd": source.project(), "timestamp": "2026-01-01T00:00:01.000Z", "message": {"role": "user", "content": "new question"}}),
        ],
    );
    source.baton(&["import", "--agent", "claude"]);
    copy_session_files(&source, &target, &session);
    target.baton(&["materialize", "--in", "paseo"]);
    assert!(!old.exists());
    assert_eq!(target.claude_transcripts().len(), 1);
    assert!(
        fs::read_to_string(target.path("paseo.log"))
            .unwrap()
            .lines()
            .any(|line| line == "delete restored-1")
    );
    let shown = target.baton(&["show", &session]);
    let host = format!(
        "@test-{}",
        target.dir.path().file_name().unwrap().to_string_lossy()
    );
    let links: Vec<_> = shown
        .lines()
        .filter(|line| line.starts_with("link     to paseo:") && line.contains(&host))
        .collect();
    assert_eq!(links.len(), 1);
    assert!(links[0].contains("paseo:restored-2"));
}

#[test]
fn paseo_export_requires_a_provider() {
    let home = Home::new();
    write_pi(&home, &[header("/work"), ask("a1", None, 1, "question")]);
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);
    let output = home.baton_output(&["export", &session, "--to", "paseo"], &[]);
    assert!(!output.status.success());
    assert_eq!(home.sessions(&[]), vec![session]);
}

#[cfg(unix)]
#[test]
fn resume_opens_the_export_in_the_resolved_project_after_unlocking_the_store() {
    use std::os::unix::fs::PermissionsExt;

    let home = Home::new();
    write_pi(
        &home,
        &[header("/work"), ask("a1", None, 1, "resume question")],
    );
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);
    let bin = home.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("claude");
    fs::write(
        &executable,
        r##"#!/bin/sh
printf '%s\n' "$2" > "$HOME/resumed-id"
pwd > "$HOME/resumed-cwd"
"$BATON_TEST_BINARY" list > "$HOME/resumed-list" &
child=$!
(sleep 5; kill "$child" 2>/dev/null) >/dev/null 2>&1 &
watchdog=$!
wait "$child"
status=$?
kill "$watchdog" 2>/dev/null
wait "$watchdog" 2>/dev/null
exit "$status"
"##,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited_path)))
            .unwrap();
    let binary = std::ffi::OsStr::new(env!("CARGO_BIN_EXE_baton"));
    home.baton_with_env(
        &["resume", &session, "--in", "claude", "--cwd", "."],
        &[("PATH", path.as_os_str()), ("BATON_TEST_BINARY", binary)],
    );

    let native_id = fs::read_to_string(home.path("resumed-id")).unwrap();
    let transcript = home.claude_transcript(native_id.trim());
    assert!(
        fs::read_to_string(transcript)
            .unwrap()
            .contains("resume question")
    );
    assert_eq!(
        fs::canonicalize(fs::read_to_string(home.path("resumed-cwd")).unwrap().trim()).unwrap(),
        fs::canonicalize(home.project()).unwrap()
    );
    assert!(
        fs::read_to_string(home.path("resumed-list"))
            .unwrap()
            .lines()
            .any(|line| line.split_whitespace().next() == Some(session.as_str()))
    );
}

fn header(cwd: &str) -> Value {
    json!({"type": "session", "version": 3, "id": "pi-1", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd})
}

fn entry(id: &str, parent: Option<&str>, second: u32, message: Value) -> Value {
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": format!("2026-01-01T00:00:{second:02}.000Z"), "message": message})
}

fn ask(id: &str, parent: Option<&str>, second: u32, text: &str) -> Value {
    entry(id, parent, second, json!({"role": "user", "content": text}))
}

fn answer(id: &str, parent: &str, second: u32, text: &str) -> Value {
    entry(
        id,
        Some(parent),
        second,
        json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"}),
    )
}

fn write_pi(home: &Home, records: &[Value]) {
    fs::create_dir_all(home.pi_file().parent().unwrap()).unwrap();
    fs::write(home.pi_file(), jsonl(records)).unwrap();
}

fn copy_session_files(from: &Home, to: &Home, session: &str) {
    fs::create_dir_all(to.path("baton")).unwrap();
    fs::copy(from.path("baton/version"), to.path("baton/version")).unwrap();
    for kind in ["events", "records"] {
        let source = from.path(&format!("baton/sessions/{session}/{kind}"));
        let destination = to.path(&format!("baton/sessions/{session}/{kind}"));
        fs::create_dir_all(&destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if !target.exists() {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}

fn git_project(path: &Path, remote: &str) {
    for args in [
        &["init", "-q"][..],
        &["remote", "add", "origin", remote][..],
    ] {
        let mut command = Command::new("git");
        command.args(args).current_dir(path);
        isolate_git(&mut command, &path.join(".isolated-gitconfig"));
        assert!(command.status().unwrap().success());
    }
}

fn transcript_cwd(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    first["cwd"].as_str().unwrap().to_owned()
}

#[test]
fn independently_imported_native_sessions_deduplicate_after_file_union() {
    let first = Home::new();
    let second = Home::new();
    let records = [
        header("/work"),
        ask("a1", None, 1, "shared question"),
        answer("a2", "a1", 2, "shared answer"),
    ];
    write_pi(&first, &records);
    write_pi(&second, &records);
    first.baton(&["import", "--agent", "pi"]);
    second.baton(&["import", "--agent", "pi"]);
    let id = first.sessions(&[]).remove(0);
    assert_eq!(second.sessions(&[]), vec![id.clone()]);

    copy_session_files(&second, &first, &id);
    assert_eq!(first.sessions(&[]), vec![id.clone()]);
    assert_eq!(first.event_count(&id), 2);
    let shown = first.baton(&["show", &id]);
    assert_eq!(shown.matches("shared question").count(), 1);
    assert_eq!(shown.matches("shared answer").count(), 1);
    assert!(
        shown.contains("created  2026-01-01T00:00:00.000Z"),
        "{shown}"
    );
    assert_eq!(shown.matches("link     from pi:pi-1").count(), 2);
    for home in [&first, &second] {
        let host = format!(
            "@test-{}",
            home.dir.path().file_name().unwrap().to_string_lossy()
        );
        assert!(shown.contains(&host), "{shown}");
    }
    assert!(!shown.contains("branch  "));
    let later_record = first.path(&format!(
        "baton/sessions/{id}/records/test-ffffffff-ffff-7fff-bfff-ffffffffffff.json"
    ));
    fs::write(later_record, r#"{"created":"2025-12-31T23:59:59.000Z"}"#).unwrap();
    let shown = first.baton(&["show", &id]);
    assert!(
        shown.contains("created  2025-12-31T23:59:59.000Z"),
        "{shown}"
    );
}

#[test]
fn independent_continuations_form_active_and_alternate_branches() {
    let first = Home::new();
    let second = Home::new();
    let records = [header("/work"), ask("a1", None, 1, "shared root")];
    write_pi(&first, &records);
    write_pi(&second, &records);
    first.baton(&["import", "--agent", "pi"]);
    second.baton(&["import", "--agent", "pi"]);
    let id = first.sessions(&[]).remove(0);
    assert_eq!(second.sessions(&[]), vec![id.clone()]);
    append(
        &first.pi_file(),
        &[ask("a2", Some("a1"), 2, "earlier continuation")],
    );
    append(
        &second.pi_file(),
        &[ask("b2", Some("a1"), 3, "later continuation")],
    );
    first.baton(&["import", "--agent", "pi"]);
    second.baton(&["import", "--agent", "pi"]);

    copy_session_files(&second, &first, &id);
    assert_eq!(first.event_count(&id), 3);
    let shown = first.baton(&["show", &id]);
    let alternate = shown
        .lines()
        .find(|line| line.starts_with("branch   "))
        .unwrap();
    assert!(
        alternate.contains("2026-01-01T00:00:02.000Z")
            && alternate.contains("earlier continuation"),
        "{shown}"
    );
    let prefix = alternate.split_whitespace().nth(1).unwrap();
    assert_eq!(prefix.len(), 8);
    assert!(shown.contains("user: later continuation"), "{shown}");
    assert!(!shown.contains("user: earlier continuation"), "{shown}");
    assert!(first.baton(&["list"]).contains(" *"));
    copy_session_files(&first, &second, &id);
    assert_eq!(second.event_count(&id), 3);
    let shown_elsewhere = second.baton(&["show", &id]);
    assert!(
        shown_elsewhere
            .lines()
            .any(|line| line.starts_with("branch   ") && line.contains("earlier continuation")),
        "{shown_elsewhere}"
    );
    assert!(
        shown_elsewhere.contains("user: later continuation"),
        "{shown_elsewhere}"
    );
}

#[test]
fn index_refreshes_when_external_segments_appear_and_disappear() {
    let source = Home::new();
    let target = Home::new();
    let mut session_header = header("/work");
    session_header["timestamp"] = Value::Null;
    let records = [session_header, ask("a1", None, 1, "initial content")];
    write_pi(&source, &records);
    write_pi(&target, &records);
    source.baton(&["import", "--agent", "pi"]);
    target.baton(&["import", "--agent", "pi"]);
    let id = target.sessions(&[]).remove(0);
    assert_eq!(source.sessions(&[]), vec![id.clone()]);
    assert_eq!(target.event_count(&id), 1);
    assert!(
        target
            .baton(&["show", &id])
            .contains("created  2026-01-01T00:00:01.000Z")
    );

    let source_events = source.path(&format!("baton/sessions/{id}/events"));
    let existing: Vec<_> = fs::read_dir(&source_events)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    append(
        &source.pi_file(),
        &[ask("a2", Some("a1"), 2, "externally added content")],
    );
    source.baton(&["import", "--agent", "pi"]);
    let added = fs::read_dir(&source_events)
        .unwrap()
        .map(|entry| entry.unwrap())
        .find(|entry| !existing.contains(&entry.file_name()))
        .unwrap();
    let copied = target
        .path(&format!("baton/sessions/{id}/events"))
        .join(added.file_name());
    fs::copy(added.path(), &copied).unwrap();
    assert_eq!(target.event_count(&id), 2);

    fs::remove_file(copied).unwrap();
    assert_eq!(target.event_count(&id), 1);
}

#[test]
fn migration_remaps_fork_parents_and_is_idempotent() {
    use baton_core::{Agent, Body, Draft, Event, User};

    let home = Home::new();
    let root_id = "018f0000-0000-7000-8000-000000000001";
    let fork_id = "018f0000-0000-7000-8000-000000000002";
    let draft = |text: &str, second: u32| Draft {
        timestamp: format!("2026-01-01T00:00:{second:02}.000Z"),
        native_entry_id: None,
        body: Body::User(User {
            text: text.into(),
            images: vec![],
        }),
    };
    let root_events = Event::chain([draft("original root", 1)], None, Agent::Pi, "root-native");
    let fork_event = Event::chain(
        [draft("fork continuation", 2)],
        Some(root_events[0].id.clone()),
        Agent::Pi,
        "fork-native",
    );
    let mut fork_events = root_events.clone();
    fork_events.extend(fork_event);
    for (id, native_id, title, parent, events) in [
        (
            root_id,
            "root-native",
            "original session",
            Value::Null,
            &root_events,
        ),
        (
            fork_id,
            "fork-native",
            "forked session",
            json!({"session": root_id, "event": root_events[0].id}),
            &fork_events,
        ),
    ] {
        let directory = home.path(&format!("baton/sessions/{id}"));
        fs::create_dir_all(&directory).unwrap();
        let meta = json!({
            "id": id, "title": title, "cwd": "/work", "created": "2026-01-01T00:00:00.000Z", "updated": events.last().unwrap().timestamp,
            "parent": parent,
            "links": [{"agent": "pi", "native_id": native_id, "path": "/old/native.jsonl", "exported_events": events.len(), "direction": "import"}]
        });
        fs::write(
            directory.join("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        let values: Vec<Value> = events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        fs::write(directory.join("events.jsonl"), jsonl(&values)).unwrap();
    }

    home.baton(&["migrate"]);
    let ids = home.sessions(&[]);
    assert_eq!(ids.len(), 2);
    let root = ids
        .iter()
        .find(|id| {
            home.baton(&["show", id])
                .contains("title    original session")
        })
        .unwrap();
    let fork = ids
        .iter()
        .find(|id| {
            home.baton(&["show", id])
                .contains("title    forked session")
        })
        .unwrap();
    assert_ne!(root.as_str(), root_id);
    assert_ne!(fork.as_str(), fork_id);
    let shown = home.baton(&["show", fork]);
    assert!(
        shown.contains("created  2026-01-01T00:00:00.000Z"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("forked   from {root} at {}", root_events[0].id)),
        "{shown}"
    );
    assert!(
        shown.contains("original root") && shown.contains("fork continuation"),
        "{shown}"
    );
    let files = |id: &str| {
        ["events", "records"]
            .into_iter()
            .map(|kind| {
                let directory = home.path(&format!("baton/sessions/{id}/{kind}"));
                let mut names: Vec<_> = fs::read_dir(directory)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                names.sort();
                names
            })
            .collect::<Vec<_>>()
    };
    let before = (files(root), files(fork));
    home.baton(&["migrate"]);
    assert_eq!(home.sessions(&[]).len(), 2);
    assert_eq!((files(root), files(fork)), before);
    assert_eq!(home.event_count(root), 1);
    assert_eq!(home.event_count(fork), 2);
}

#[test]
fn import_reads_appended_lines_once_and_follows_branches() {
    let home = Home::new();
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "first question"),
            answer("a2", "a1", 2, "first answer"),
        ],
    );
    home.baton(&["import"]);
    assert!(
        fs::read_dir(home.path("baton")).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();

            name == "sessions" || name == "blobs" || name == ".lock.tmp" || name == "version"
        }),
        "machine-local state stays out of BATON_HOME"
    );
    let lock = fs::metadata(home.path("baton/.lock.tmp")).unwrap();
    assert!(lock.is_file());
    assert_eq!(lock.len(), 0);
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    assert_eq!(home.event_count(&session), 2);

    append(
        &home.pi_file(),
        &[
            ask("a3", Some("a2"), 3, "second question"),
            answer("a4", "a3", 4, "second answer"),
        ],
    );
    home.baton(&["import"]);
    assert_eq!(home.event_count(&session), 4);

    append(
        &home.pi_file(),
        &[ask("b1", Some("a2"), 5, "branch question")],
    );
    home.baton(&["import"]);
    assert_eq!(home.event_count(&session), 5);
    let shown = home.baton(&["show", &session]);
    assert!(
        shown.contains("branch question") && !shown.contains("second question"),
        "{shown}"
    );

    let text = fs::read_to_string(home.pi_file()).unwrap();
    fs::write(
        home.pi_file(),
        text.replace("\"cwd\":\"/work\"", "\"cwd\":\"/moved\""),
    )
    .unwrap();
    home.baton(&["import"]);
    assert_eq!(home.event_count(&session), 5);
}

#[test]
fn exported_sessions_flow_back_and_a_diverged_source_forks() {
    let home = Home::new();
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "question one"),
            answer("a2", "a1", 2, "answer one"),
        ],
    );
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    let claude_id = home.export(&session, "claude");
    home.baton(&["import"]);
    assert_eq!(home.sessions(&[]), vec![session.clone()]);
    assert_eq!(home.event_count(&session), 2);

    home.continue_claude(&claude_id, ("c1", "question two"), ("c2", "answer two"));
    home.baton(&["import", "--agent", "claude"]);
    assert_eq!(home.event_count(&session), 4);
    assert!(home.baton(&["show", &session]).contains("answer two"));

    append(
        &home.pi_file(),
        &[ask("a3", Some("a2"), 3, "pi only question")],
    );
    home.baton(&["import", "--agent", "pi"]);
    let sessions = home.sessions(&[]);
    assert_eq!(sessions.len(), 2);
    let fork = sessions.iter().find(|id| *id != &session).unwrap();
    let shown = home.baton(&["show", fork]);
    assert!(
        shown.contains(&format!("forked   from {session}")),
        "{shown}"
    );
    assert!(
        shown.contains("pi only question")
            && shown.contains("answer one")
            && !shown.contains("answer two")
    );
    home.baton(&["import"]);
    assert_eq!(home.sessions(&[]).len(), 2);
    assert!(
        home.baton(&["show", &format!("claude:{}", &claude_id[..8])])
            .contains(&session)
    );
}

#[test]
fn two_continued_exports_of_one_session_update_it_once_and_fork_once() {
    let home = Home::new();
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "question one"),
            answer("a2", "a1", 2, "answer one"),
        ],
    );
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    let first = home.export(&session, "claude");
    let second = home.export(&session, "claude");
    home.continue_claude(&first, ("f1", "first continuation"), ("f2", "first answer"));
    home.continue_claude(
        &second,
        ("s1", "second continuation"),
        ("s2", "second answer"),
    );

    home.baton(&["import", "--agent", "claude"]);
    let sessions = home.sessions(&[]);
    assert_eq!(sessions.len(), 2);
    let shown: Vec<_> = sessions
        .iter()
        .map(|id| home.baton(&["show", id]))
        .collect();
    assert!(
        shown
            .iter()
            .any(|text| text.contains("first continuation") && text.contains("first answer"))
    );
    assert!(
        shown
            .iter()
            .any(|text| text.contains("second continuation") && text.contains("second answer"))
    );
    assert!(
        shown
            .iter()
            .any(|text| text.contains(&format!("forked   from {session}")))
    );
}

#[test]
fn exports_record_the_absolute_project_directory() {
    let home = Home::new();
    write_pi(&home, &[header("/work"), ask("a1", None, 1, "question")]);
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    let claude_id = home.export(&session, "claude");
    let text = fs::read_to_string(home.claude_transcript(&claude_id)).unwrap();
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let cwd = first["cwd"].as_str().unwrap();
    assert!(
        Path::new(cwd).is_absolute() && cwd.ends_with("project"),
        "`--cwd .` reaches the agent as the project directory: {cwd}"
    );
}

#[test]
fn materialize_recreates_a_wiped_export_without_new_records() {
    let home = Home::new();
    write_pi(
        &home,
        &[header("/work"), ask("a1", None, 1, "materialized question")],
    );
    home.baton(&["import", "--agent", "pi"]);
    let session = home.sessions(&[]).remove(0);

    home.baton(&["materialize", "--in", "claude"]);
    let first = home.claude_transcripts();
    assert_eq!(first.len(), 1);
    let contents = fs::read(&first[0]).unwrap();
    assert!(String::from_utf8_lossy(&contents).contains("materialized question"));
    home.baton(&["materialize", "--in", "claude"]);
    assert_eq!(home.claude_transcripts(), first);
    assert_eq!(fs::read(&first[0]).unwrap(), contents);
    let records = home.path(&format!("baton/sessions/{session}/records"));
    let record_files = || {
        let mut files: Vec<_> = fs::read_dir(&records)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        files.sort();
        files
    };
    let before = record_files();

    fs::remove_dir_all(home.path(".claude")).unwrap();
    home.baton(&["materialize", "--in", "claude"]);
    let recreated = home.claude_transcripts();
    assert_eq!(recreated, first);
    assert_eq!(record_files(), before);
    assert!(
        fs::read_to_string(&recreated[0])
            .unwrap()
            .contains("materialized question")
    );
    home.baton(&["materialize", "--in", "claude"]);
    assert_eq!(home.claude_transcripts(), recreated);
    assert_eq!(record_files(), before);
}

#[test]
fn materialize_refreshes_untouched_export_after_cross_host_union() {
    let first = Home::new();
    let second = Home::new();
    let records = [header("/work"), ask("a1", None, 1, "initial question")];
    write_pi(&first, &records);
    write_pi(&second, &records);
    first.baton(&["import", "--agent", "pi"]);
    second.baton(&["import", "--agent", "pi"]);
    let session = first.sessions(&[]).remove(0);
    first.baton(&["materialize", "--in", "claude"]);
    let stale = first.claude_transcripts().remove(0);

    append(
        &second.pi_file(),
        &[answer("a2", "a1", 2, "remote continuation")],
    );
    second.baton(&["import", "--agent", "pi"]);
    copy_session_files(&second, &first, &session);
    first.baton(&["materialize", "--in", "claude"]);

    let current = first.claude_transcripts();
    assert_eq!(current.len(), 1);
    assert!(!stale.exists());
    assert!(
        fs::read_to_string(&current[0])
            .unwrap()
            .contains("remote continuation")
    );
    first.baton(&["materialize", "--in", "claude"]);
    assert_eq!(first.claude_transcripts(), current);
}

#[test]
fn materialize_preserves_continued_exports_and_imported_originals() {
    let first = Home::new();
    let second = Home::new();
    let native_id = "11111111-1111-4111-8111-111111111111";
    let record = |uuid: &str, parent: Option<&str>, text: &str| {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": native_id, "isSidechain": false,
               "cwd": "/work", "timestamp": "2026-01-01T00:00:00.000Z", "message": {"role": "user", "content": text}})
    };
    for home in [&first, &second] {
        let path = home.path(&format!(".claude/projects/-work/{native_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, jsonl(&[record("a1", None, "original question")])).unwrap();
        home.baton(&["import", "--agent", "claude"]);
    }
    let session = first.sessions(&[]).remove(0);
    let original = first.path(&format!(".claude/projects/-work/{native_id}.jsonl"));
    let exported_id = first.export(&session, "claude");
    let continued = first
        .claude_transcripts()
        .into_iter()
        .find(|path| path.file_stem().unwrap() == exported_id.as_str())
        .unwrap();
    let contents = fs::read_to_string(&continued).unwrap();
    let last = contents
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()?
                .get("uuid")?
                .as_str()
                .map(String::from)
        })
        .next_back()
        .unwrap();
    append(
        &continued,
        &[
            json!({"type": "user", "uuid": "local-continuation", "parentUuid": last, "sessionId": exported_id,
                               "isSidechain": false, "cwd": first.project(), "timestamp": "2026-01-02T00:00:00.000Z",
                               "message": {"role": "user", "content": "local continuation"}}),
        ],
    );
    let continued_contents = fs::read(&continued).unwrap();
    let original_contents = fs::read(&original).unwrap();

    let remote = second.path(&format!(".claude/projects/-work/{native_id}.jsonl"));
    append(&remote, &[record("a2", Some("a1"), "remote continuation")]);
    second.baton(&["import", "--agent", "claude"]);
    copy_session_files(&second, &first, &session);
    first.baton(&["materialize", "--in", "claude"]);

    assert_eq!(fs::read(&original).unwrap(), original_contents);
    assert_eq!(fs::read(&continued).unwrap(), continued_contents);
    let transcripts = first.claude_transcripts();
    assert_eq!(transcripts.len(), 3);
    assert!(transcripts.iter().any(|path| {
        path != &original
            && path != &continued
            && fs::read_to_string(path)
                .unwrap()
                .contains("remote continuation")
    }));
    first.baton(&["materialize", "--in", "claude"]);
    assert_eq!(first.claude_transcripts(), transcripts);
}

#[test]
fn project_identity_and_path_mapping_choose_materialization_directory() {
    let source = Home::new();
    let matching = Home::new();
    let mapped = Home::new();
    let recorded = Home::new();
    let remote = "https://example.com/team/project.git";
    git_project(&source.project(), remote);
    git_project(&matching.project(), remote);
    let source_cwd = fs::canonicalize(source.project()).unwrap();
    let matching_cwd = fs::canonicalize(matching.project()).unwrap();
    let mapped_cwd = mapped.path("mapped-project");
    fs::create_dir_all(&mapped_cwd).unwrap();
    let mapped_cwd = fs::canonicalize(mapped_cwd).unwrap();
    write_pi(
        &source,
        &[
            header(source_cwd.to_str().unwrap()),
            ask("a1", None, 1, "repository question"),
        ],
    );
    source.baton(&["import", "--agent", "pi"]);
    let session = source.sessions(&[]).remove(0);
    for destination in [&matching, &mapped, &recorded] {
        copy_session_files(&source, destination, &session);
    }

    assert!(
        matching
            .baton_with_env(
                &["list", "--cwd"],
                &[("BATON_PROJECTS", matching_cwd.as_os_str())]
            )
            .contains(&session)
    );
    let matching_map =
        std::env::join_paths([format!("{}={}", source_cwd.display(), mapped_cwd.display())])
            .unwrap();
    matching.baton_with_env(
        &["materialize", "--in", "claude"],
        &[
            ("BATON_PATH_MAP", matching_map.as_os_str()),
            ("BATON_PROJECTS", matching_cwd.as_os_str()),
        ],
    );
    assert_eq!(
        transcript_cwd(&matching.claude_transcripts()[0]),
        matching_cwd.to_str().unwrap()
    );

    mapped.baton_with_env(
        &["materialize", "--in", "claude"],
        &[("BATON_PATH_MAP", matching_map.as_os_str())],
    );
    assert_eq!(
        transcript_cwd(&mapped.claude_transcripts()[0]),
        mapped_cwd.to_str().unwrap()
    );

    recorded.baton(&["materialize", "--in", "claude"]);
    assert_eq!(
        transcript_cwd(&recorded.claude_transcripts()[0]),
        source_cwd.to_str().unwrap()
    );
}

#[test]
fn an_emptied_export_leaves_its_session_alone() {
    let home = Home::new();
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "question"),
            answer("a2", "a1", 2, "answer"),
        ],
    );
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    let claude_id = home.export(&session, "claude");
    fs::write(home.claude_transcript(&claude_id), "").unwrap();
    home.baton(&["import"]);
    assert_eq!(home.sessions(&[]), vec![session.clone()]);
    assert_eq!(home.event_count(&session), 2);
}

#[test]
fn exports_import_back_unchanged_with_and_without_cursors() {
    let home = Home::new();
    let image = "cGl4ZWxz";
    write_pi(
        &home,
        &[
            header("/work"),
            entry(
                "a1",
                None,
                1,
                json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "data": image, "mimeType": "image/png"}]}),
            ),
            entry(
                "a2",
                Some("a1"),
                2,
                json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm", "thinkingSignature": "sig"},
                {"type": "text", "text": "reading"},
                {"type": "toolCall", "id": "call|1", "name": "read", "arguments": {"path": "a"}},
                {"type": "toolCall", "id": "call|2", "name": "bash", "arguments": {"command": "false"}},
            ], "stopReason": "toolUse"}),
            ),
            entry(
                "a3",
                Some("a2"),
                3,
                json!({"role": "toolResult", "toolCallId": "call|1", "toolName": "read", "content": [{"type": "text", "text": "contents"}, {"type": "image", "data": image, "mimeType": "image/png"}], "isError": false}),
            ),
            entry(
                "a4",
                Some("a3"),
                4,
                json!({"role": "toolResult", "toolCallId": "call|2", "toolName": "bash", "content": [{"type": "text", "text": "failed"}], "isError": true}),
            ),
            json!({"type": "compaction", "id": "c1", "parentId": "a4", "timestamp": "2026-01-01T00:00:05.000Z", "summary": "summary", "firstKeptEntryId": "a1", "tokensBefore": 1}),
            ask("a5", Some("c1"), 6, "after compaction"),
            answer("a6", "a5", 7, "done"),
            json!({"type": "session_info", "id": "s1", "parentId": "a6", "timestamp": "2026-01-01T00:00:08.000Z", "name": "Round trip"}),
        ],
    );
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    for agent in ["pi", "omp", "claude", "codex"] {
        home.export(&session, agent);
    }
    let count = home.event_count(&session);
    home.baton(&["import"]);
    assert_eq!(home.sessions(&[]), vec![session.clone()]);
    assert_eq!(home.event_count(&session), count);
    let listed = home.baton(&["list", "--agent", "codex"]);
    assert!(
        listed.contains(&session) && listed.contains("Round trip"),
        "{listed}"
    );
    fs::remove_dir_all(home.path("cache")).unwrap();
    home.baton(&["import"]);
    assert_eq!(home.sessions(&[]), vec![session.clone()]);
    assert_eq!(home.event_count(&session), count);
}

#[test]
fn sessions_filter_by_agent_and_resolve_by_prefix_or_native_id() {
    let home = Home::new();
    write_pi(
        &home,
        &[
            header("/work"),
            ask("a1", None, 1, "where is the 配置文件"),
            answer("a2", "a1", 2, "in the repo"),
        ],
    );
    home.baton(&["import"]);
    let session = home.sessions(&[]).remove(0);
    assert!(home.baton(&["list", "--agent", "pi"]).contains(&session));
    assert!(!home.baton(&["list", "--agent", "codex"]).contains(&session));
    assert!(
        home.baton(&["show", &session[..13]])
            .contains("where is the")
    );
    assert!(
        home.baton(&["show", "pi:pi-1"])
            .contains(&format!("id       {session}"))
    );
}
