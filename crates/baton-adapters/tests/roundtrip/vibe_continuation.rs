//! A session baton exports to Vibe and Vibe 3 continues in its Unified store reads back under the exported id, as the export followed by the continuation, and flows back into the session it came from
//!
//! The stores are written by hand in the layout Vibe 3.1 uses: a `CURRENT` pointer, a manifest naming the generation's documents by digest, a checkpoint whose context messages live in a chunk, and the recovery journal

use crate::support;

use std::path::{Path, PathBuf};
use std::{env, fs};

use baton_adapters::VibeAdapter;
use baton_core::{
    Adapter, Agent, Baton, Blobs, Body, Event, Export, Image, ImportOutcome, NativeSession, Source,
    replay,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// The Unified session Vibe imported the exported folder into
const STORE: &str = "019a4c1e-7b2f-7c3d-9e4f-5a6b7c8d9e0f";
const CREATED: &str = "2026-01-01T09:00:00.000Z";
// When Vibe published the generation holding the continuation
const SAVED: &str = "2026-01-02T10:00:00.000Z";
// When Vibe created a session of its store, which the store keeps in Unix milliseconds
const STARTED: &str = "2026-01-02T08:00:00.000Z";
const STARTED_MILLIS: i64 = 1_767_340_800_000;
// The image bytes `pixels` in base64, the way Vibe embeds images
const PIXELS: &str = "cGl4ZWxz";
const ROOT: &str = "BATON_VIBE_CONTINUATION_ROOT";

fn pixels(blobs: &Blobs) -> Image {
    Image {
        blob: blobs.put(b"pixels").unwrap(),
        mime: "image/png".into(),
    }
}

// The first call is answered only after the second, and the second never, so baton writes lines that only its own reader drops, and keeps the result's image where Vibe has no field for it
fn conversation(blobs: &Blobs) -> Vec<Body> {
    vec![
        support::user("look at this", vec![pixels(blobs)]),
        support::assistant("reading"),
        support::call("call_1", "read_file", json!({"path": "a.txt"})),
        support::call("call_2", "bash", json!({"command": "sleep 9"})),
        support::result("call_1", "a.txt is empty", vec![pixels(blobs)], true),
        support::user("go on", vec![]),
        support::assistant("done"),
    ]
}

// Writes `bodies` as baton exports them to Vibe, and returns the written session with the bodies the export replays
fn export(adapter: &VibeAdapter, blobs: &Blobs, bodies: Vec<Body>) -> (NativeSession, Vec<Body>) {
    let events = Event::chain(
        bodies.into_iter().map(support::draft),
        None,
        Agent::Pi,
        "source",
    );
    let path: Vec<&Event> = events.iter().collect();
    let replayed = replay::events(&path);
    let models = replay::Models::new(&path);
    let export = Export {
        title: Some("exported"),
        cwd: Path::new("/work/demo"),
        created: CREATED,
        events: &replayed,
        models: &models,
        seed: None,
    };
    let native = adapter.write(&export, blobs).unwrap();
    let bodies = replayed.iter().map(|event| event.body.clone()).collect();
    (native, bodies)
}

// The context messages Vibe's import makes of an exported folder, which keep none of baton's own fields
fn imported(messages: &Path) -> Vec<Value> {
    let text = fs::read_to_string(messages).unwrap();
    text.lines()
        .map(|line| {
            let line: Value = serde_json::from_str(line).unwrap();
            let mut content = vec![json!({"type": "text", "text": line["content"]})];
            let message = match line["role"].as_str().unwrap() {
                "user" => {
                    for image in line["images"].as_array().into_iter().flatten() {
                        content.push(json!({"type": "image", "data": image["source"]["data"], "mimeType": image["mime_type"]}));
                    }
                    json!({"role": "user", "content": content})
                }
                "assistant" => {
                    for call in line["tool_calls"].as_array().into_iter().flatten() {
                        let raw = &call["function"]["arguments"];
                        let value: Value = serde_json::from_str(raw.as_str().unwrap()).unwrap();
                        content.push(json!({"type": "tool_call", "id": call["id"], "name": call["function"]["name"],
                                            "arguments": {"type": "json", "raw": raw, "value": value}}));
                    }
                    json!({"role": "assistant", "content": content})
                }
                // Results wrapped in an error or cancellation tag count as failed
                _ => {
                    let failed = line["content"].as_str().unwrap().starts_with('<');
                    json!({"role": "tool", "tool_call_id": line["tool_call_id"], "name": line["name"],
                           "outcome": if failed { "failure" } else { "success" }, "content": content})
                }
            };
            json!({"message": message, "source": "history", "turn_index": 0})
        })
        .collect()
}

fn stored(source: &str, message: Value) -> Value {
    json!({"message": message, "source": source, "turn_index": 1})
}

fn user_text(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

// A turn Vibe 3 added: a message with an image and injected context, a reply that thinks and calls a tool, the call failing with a screenshot, and the answer
fn continuation() -> Vec<Value> {
    vec![
        stored(
            "history",
            json!({"role": "user", "content": [{"type": "text", "text": "and now?"}, {"type": "image", "data": PIXELS, "mimeType": "image/png"}]}),
        ),
        stored(
            "injection",
            json!({"role": "user", "content": [{"type": "text", "text": "<system-reminder>be brief</system-reminder>"}], "context": true}),
        ),
        stored(
            "history",
            json!({"role": "assistant", "content": [
                {"type": "reasoning", "content": [{"type": "text", "text": "thinking it over"}], "_meta": {"vibe_reasoning_payloads": [{"id": "rs_1"}]}},
                {"type": "text", "text": "checking"},
                {"type": "tool_call", "id": "call_3", "name": "read_file", "arguments": {"type": "json", "raw": "{\"path\":\"b.txt\"}", "value": {"path": "b.txt"}}},
            ]}),
        ),
        stored(
            "history",
            json!({"role": "tool", "tool_call_id": "call_3", "name": "read_file", "outcome": "failure", "content": [
                {"type": "text", "text": "File not found"},
                {"type": "resource", "resource": {"uri": "file:///tmp/shot.png", "mimeType": "image/png", "blob": PIXELS}},
            ]}),
        ),
        stored(
            "history",
            json!({"role": "assistant", "content": [{"type": "text", "text": "b.txt is missing"}]}),
        ),
    ]
}

fn continued_bodies(blobs: &Blobs) -> Vec<Body> {
    vec![
        support::user("and now?", vec![pixels(blobs)]),
        support::thinking("thinking it over", Some(r#"[{"id":"rs_1"}]"#)),
        support::assistant("checking"),
        support::call("call_3", "read_file", json!({"path": "b.txt"})),
        support::result("call_3", "File not found", vec![pixels(blobs)], true),
        support::assistant("b.txt is missing"),
    ]
}

// What Vibe records about the legacy folder it imports, whose revision covers both of its files
fn provenance(folder: &Path, imported_at: &str) -> Value {
    let meta = fs::read(folder.join("meta.json")).unwrap();
    let messages = fs::read(folder.join("messages.jsonl")).unwrap();
    let mut revision = Sha256::new();
    for part in [
        b"metadata\0".as_slice(),
        &meta,
        b"\0messages\0",
        &messages,
        b"\0",
    ] {
        revision.update(part);
    }
    let id = serde_json::from_slice::<Value>(&meta).unwrap()["session_id"].clone();
    json!({"source": {"backend": "legacy", "session_id": id, "store_revision": base16ct::lower::encode_string(&revision.finalize())},
           "history_sha256": "0".repeat(64), "imported_at": imported_at})
}

// Vibe stores each document as JSON and a newline, and records the SHA-256 of the JSON
fn store(path: &Path, value: &Value) -> String {
    let body = value.to_string();
    support::write(path, &format!("{body}\n"));
    base16ct::lower::encode_string(&Sha256::digest(body.as_bytes()))
}

// Session `id` of the Unified store in the session log directory `sessions`, published as its fourth generation
fn unified_store(
    sessions: &Path,
    id: &str,
    provenance: Value,
    messages: Vec<Value>,
    journal: &[Value],
) -> PathBuf {
    let dir = sessions.join("unified").join(id);
    let generation = dir.join("generations/0000000000000004");
    let transcript = json!(messages);
    let digest = base16ct::lower::encode_string(&Sha256::digest(transcript.to_string().as_bytes()));
    let chunk = store(&dir.join(format!("chunks/{digest}.json")), &transcript);
    let checkpoint = store(
        &generation.join("checkpoint.json"),
        &json!({"checkpoint_version": 1, "compaction_count": 0, "context": {"messages": []}, "notifications": [],
                "turn": {"type": "terminal", "turn_id": "turn-1", "outcome": {"type": "completed"}}}),
    );
    let runtime = store(
        &generation.join("runtime-state.json"),
        &json!({"runtime_state_version": 3, "session_id": id, "snapshot_sequence": 4, "storage_lifetime": "persistent",
                "session_metadata": {"cwd": "/work/demo", "root_session_id": id, "parent_session_id": null},
                "identity": {"kind": "root", "session_id": id, "root_session_id": id, "parent_session_id": null, "depth": 0},
                "import_provenance": provenance, "core_task_id": null, "command_receipts": [], "actions": [], "callbacks": [],
                "provider_operations": [], "processes": [], "submitted_process_notifications": [], "children": []}),
    );
    let projection = store(
        &generation.join("projection-state.json"),
        &json!({"projection_state_version": 1, "session_id": id, "snapshot_sequence": 4, "watermark": 4,
                "snapshot": {"format": "harness.public-session-state/v1",
                             "session": {"id": id, "title": "Vibe's automatic title", "titleSource": "auto", "preview": "look at this",
                                         "status": {"type": "idle"}, "createdAt": STARTED_MILLIS, "updatedAt": STARTED_MILLIS},
                             "history": {"entries": [], "cursor": {"after": null, "before": null}, "range": "latest"},
                             "turnQueue": {"items": [], "paused": false, "maxItems": 50}}}),
    );
    support::write(
        &dir.join("journal/0000000000000005.jsonl"),
        &support::jsonl(journal),
    );
    let manifest = store(
        &generation.join("manifest.json"),
        &json!({"manifest_version": 1, "session_id": id, "generation": "0000000000000004", "created_at": SAVED,
                "snapshot_sequence": 4, "execution_state": "quiescent", "interop_export": null,
                "checkpoint": {"path": "checkpoint.json", "sha256": checkpoint, "checkpoint_version": 1, "chunks": [chunk]},
                "runtime_state": {"path": "runtime-state.json", "sha256": runtime},
                "projection_state": {"path": "projection-state.json", "sha256": projection},
                "recovery_journal_segment": {"path": "journal/0000000000000005.jsonl", "first_sequence": 5}}),
    );
    store(
        &dir.join("CURRENT"),
        &json!({"store_format": "mistral.vibe.unified-session-store/v1", "store_format_minor": 10, "session_id": id,
                "generation": "0000000000000004", "snapshot_sequence": 4, "manifest_sha256": manifest}),
    );
    dir
}

fn record(sequence: u64, kind: &str, payload: Value) -> Value {
    json!({"recovery_journal_record_version": 1, "sequence": sequence, "previous_record_sha256": null,
           "record_sha256": "0".repeat(64), "type": kind, "payload": payload})
}

// An export of `conversation` that Vibe imported into `STORE`, continued with `messages`, and journaled `journal` after
fn continued(
    dir: &Path,
    messages: Vec<Value>,
    journal: &[Value],
) -> (VibeAdapter, Blobs, NativeSession, Vec<Body>) {
    let blobs = Blobs::new(dir.join("blobs"));
    let sessions = dir.join("vibe");
    let adapter = VibeAdapter::new(sessions.clone());
    let (exported, bodies) = export(&adapter, &blobs, conversation(&blobs));
    let mut context = imported(&exported.path);
    context.extend(messages);
    let folder = exported.path.parent().unwrap();
    unified_store(
        &sessions,
        STORE,
        provenance(folder, "2026-01-02T09:00:00.000Z"),
        context,
        journal,
    );
    (adapter, blobs, exported, bodies)
}

#[test]
fn a_continued_export_reads_as_the_export_followed_by_the_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, blobs, exported, mut expected) = continued(dir.path(), continuation(), &[]);
    let natives = adapter.discover(None).unwrap();
    assert_eq!(natives.len(), 1, "the store takes the place of the folder");
    let native = &natives[0];
    assert_eq!(native.id, exported.id);
    assert_eq!(
        native.path,
        dir.path().join("vibe/unified").join(STORE).join("CURRENT")
    );
    let read = adapter.read(native, None, &blobs).unwrap();
    expected.extend(continued_bodies(&blobs));
    assert_eq!(support::bodies(&read.drafts), expected);
    assert!(!read.continued);
    assert_eq!(read.drafts.last().unwrap().timestamp, SAVED);
    assert_eq!(
        read.info.title.as_deref(),
        Some("exported"),
        "an automatic title does not replace the folder's"
    );
    assert_eq!(read.info.created.as_deref(), Some(CREATED));
    assert_eq!(read.info.cwd.as_deref(), Some("/work/demo"));
    let command = adapter.resume_command(native, Path::new("/work/demo"));
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["--resume", STORE]);
    assert!(adapter.remove(native).is_err());
    assert!(native.path.is_file(), "baton leaves Vibe's store alone");
}

#[test]
fn a_compaction_in_vibe_follows_the_export_as_one_summary() {
    let dir = tempfile::tempdir().unwrap();
    let preamble =
        "You are continuing a trajectory after a context compaction.\n\n<previous_user_messages>";
    let summary = "</previous_user_messages>\n\nHere is a summary of what has happened so far:\n\n<compaction_summary>\nread a.txt\n</compaction_summary>";
    let view = "<system>Items after the summary, shown reduced because they did not fit:</system>\nUserMessage: and now?";
    let compacted = vec![
        stored("injection", user_text(preamble)),
        stored("history", user_text("look at this")),
        stored("injection", user_text(summary)),
        stored("injection", user_text(view)),
        stored(
            "history",
            json!({"role": "assistant", "content": [{"type": "text", "text": "b.txt is missing"}]}),
        ),
        stored("history", user_text("thanks")),
    ];
    let (adapter, blobs, exported, mut expected) = continued(dir.path(), compacted, &[]);
    let native = adapter.discover(None).unwrap().remove(0);
    assert_eq!(native.id, exported.id);
    let read = adapter.read(&native, None, &blobs).unwrap();
    expected.extend([
        support::compaction(&[preamble, "look at this", summary, view].join("\n")),
        support::assistant("b.txt is missing"),
        support::user("thanks", vec![]),
    ]);
    assert_eq!(support::bodies(&read.drafts), expected);
}

#[test]
fn a_turn_vibe_has_not_saved_is_refused_while_a_journaled_rename_reads() {
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"input": {"protocol_version": 1, "input_id": 7, "determinism": {"time_unix_ms": 0, "random_seed": 0},
                                 "command": {"type": "user_message", "turn_id": "turn-2", "content": [{"type": "text", "text": "one more"}]}},
                       "transition_sha256": "0".repeat(64)});
    let (adapter, blobs, _, _) = continued(
        dir.path(),
        continuation(),
        &[record(5, "core_input", input)],
    );
    let native = adapter.discover(None).unwrap().remove(0);
    let Err(_) = adapter.read(&native, None, &blobs) else {
        panic!("a journaled turn is read only once Vibe saved it");
    };

    let dir = tempfile::tempdir().unwrap();
    let rename = json!({"watermark": 5, "delta": [{"op": "set_envelope", "state": {"session": {"id": STORE, "title": "Renamed in Vibe", "titleSource": "manual"}}}]});
    let (adapter, blobs, _, _) = continued(
        dir.path(),
        continuation(),
        &[record(5, "projection_delta", rename)],
    );
    let native = adapter.discover(None).unwrap().remove(0);
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(read.info.title.as_deref(), Some("Renamed in Vibe"));
}

#[test]
fn a_store_newer_than_baton_reads_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, blobs, _, _) = continued(dir.path(), continuation(), &[]);
    let current = dir.path().join("vibe/unified").join(STORE).join("CURRENT");
    let pointer = fs::read_to_string(&current).unwrap();
    fs::write(
        &current,
        pointer.replace(r#""store_format_minor":10"#, r#""store_format_minor":11"#),
    )
    .unwrap();
    let native = adapter.discover(None).unwrap().remove(0);
    let Err(_) = adapter.read(&native, None, &blobs) else {
        panic!("a store revision past the known ones is refused");
    };
}

#[test]
fn vibe_resumes_a_folder_as_its_first_import_and_its_own_sessions_by_their_id() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, blobs, exported, _) = continued(dir.path(), continuation(), &[]);
    let sessions = dir.path().join("vibe");
    let later = "019a4c1e-7b2f-7c3d-9e4f-000000000002";
    let folder = exported.path.parent().unwrap();
    unified_store(
        &sessions,
        later,
        provenance(folder, "2026-01-03T09:00:00.000Z"),
        imported(&exported.path),
        &[],
    );
    let started = "019a4c1e-7b2f-7c3d-9e4f-000000000003";
    unified_store(&sessions, started, Value::Null, continuation(), &[]);
    let mut natives: Vec<(String, PathBuf)> = adapter
        .discover(None)
        .unwrap()
        .into_iter()
        .map(|native| (native.id, native.path))
        .collect();
    natives.sort();
    let current = |id: &str| sessions.join("unified").join(id).join("CURRENT");
    let mut expected = vec![
        (exported.id.clone(), current(STORE)),
        (later.to_owned(), current(later)),
        (started.to_owned(), current(started)),
    ];
    expected.sort();
    assert_eq!(natives, expected);

    let native = adapter
        .discover(Some(&sessions.join("unified").join(started)))
        .unwrap()
        .remove(0);
    assert_eq!(native.id, started);
    let from_current = adapter.discover(Some(&current(started))).unwrap();
    assert_eq!(from_current.len(), 1);
    assert_eq!(from_current[0].id, native.id);
    assert_eq!(from_current[0].path, native.path);
    assert_eq!(from_current[0].stamp, native.stamp);
    assert_eq!(from_current[0].title, native.title);
    let read = adapter.read(&native, None, &blobs).unwrap();
    assert_eq!(support::bodies(&read.drafts), continued_bodies(&blobs));
    assert_eq!(read.info.title.as_deref(), Some("Vibe's automatic title"));
    assert_eq!(read.info.created.as_deref(), Some(STARTED));
}

#[test]
fn vibe_discovers_a_legacy_session_through_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = VibeAdapter::new(dir.path().join("vibe"));
    let blobs = Blobs::new(dir.path().join("blobs"));
    let (exported, _) = export(&adapter, &blobs, conversation(&blobs));
    for path in [&exported.path, &exported.path.with_file_name("meta.json")] {
        let legacy = adapter.discover(Some(path)).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].id, exported.id);
        assert_eq!(legacy[0].path, exported.path);
    }
}

// Opens baton on data of its own, which only a separate process can point it to
#[test]
fn a_continued_export_flows_back_into_the_session_it_was_exported_from() {
    let Some(root) = env::var_os(ROOT) else {
        let root = tempfile::tempdir().unwrap();
        support::run_in_child(
            "vibe_continuation::a_continued_export_flows_back_into_the_session_it_was_exported_from",
            None,
            |command| {
                command
                    .env(ROOT, root.path())
                    .env("BATON_HOME", root.path().join("data"))
                    .env("BATON_CACHE_DIR", root.path().join("cache"))
                    .env("BATON_HOST", "vibe-test")
                    .env_remove("BATON_PROJECTS")
                    .env_remove("BATON_PATH_MAP");
            },
        );
        return;
    };
    let root = PathBuf::from(root);
    let mut baton = Baton::open().unwrap();
    let blobs = Blobs::new(root.join("blobs"));
    let source = VibeAdapter::new(root.join("source"));
    let (written, _) = export(&source, &blobs, conversation(&blobs));
    let ImportOutcome::Created { session } = baton.import(&source, &written).unwrap() else {
        panic!("expected a new session");
    };
    baton.refresh().unwrap();
    let target = VibeAdapter::new(root.join("target"));
    let exported = baton
        .export(&session, &target, Some(Path::new("/work/demo")))
        .unwrap();
    baton.refresh().unwrap();
    let mut context = imported(&exported.native.path);
    context.extend(continuation());
    let folder = exported.native.path.parent().unwrap();
    unified_store(
        &root.join("target"),
        STORE,
        provenance(folder, "2026-01-02T09:00:00.000Z"),
        context,
        &[],
    );
    let native = target.discover(None).unwrap().remove(0);
    assert_eq!(native.id, exported.native.id);
    let outcome = baton.import(&target, &native).unwrap();
    assert!(
        matches!(&outcome, ImportOutcome::Appended { session: appended, events: 6 } if *appended == session),
        "{outcome:?}"
    );
}
