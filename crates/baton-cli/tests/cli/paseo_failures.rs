//! Failures the `baton` binary must report without losing work: partly failing imports, bad paths and backups, ambiguous or never imported Paseo references, and the registrations and provider sessions of Paseo agents while materializing, with a fake `paseo` command standing in for the daemon
#![cfg(unix)]

use std::fs;

use super::support::{Home, append, claude_session, jsonl, last_claude_uuid};
use serde_json::json;

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PI_SESSION: &str = ".pi/agent/sessions/--project--/2026-01-01T00-00-00-000Z_pi-1.jsonl";

fn pi_session(home: &Home) -> String {
    let file = home.path(PI_SESSION);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        jsonl(&[
            json!({"type": "session", "version": 3, "id": "pi-1", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": home.project()}),
            json!({"type": "message", "id": "a1", "parentId": null, "timestamp": "2026-01-01T00:00:01.000Z", "message": {"role": "user", "content": "question"}}),
        ]),
    )
    .unwrap();
    home.baton(&["import", "--agent", "pi"]);
    home.sessions(&[]).remove(0)
}

// Moves the session past everything exported from it
fn answer_in_pi(home: &Home) {
    append(
        &home.path(PI_SESSION),
        &[
            json!({"type": "message", "id": "a2", "parentId": "a1", "timestamp": "2026-01-01T00:00:02.000Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "answer"}], "stopReason": "stop"}}),
        ],
    );
    home.baton(&["import", "--agent", "pi"]);
}

// Continues the only Claude Code transcript, as chatting with the Paseo agent wrapping it does
fn continue_claude(home: &Home) {
    let transcript = home.claude_transcripts().remove(0);
    let last = last_claude_uuid(&transcript);
    let id = transcript.file_stem().unwrap().to_str().unwrap();
    append(
        &transcript,
        &[
            json!({"type": "user", "uuid": "c1", "parentUuid": last, "sessionId": id, "isSidechain": false, "cwd": home.project(), "timestamp": "2026-01-02T00:00:00.000Z", "message": {"role": "user", "content": "continued in Paseo"}}),
        ],
    );
}

fn paseo_agent(home: &Home, id: &str, provider: &str, handle: &str) {
    let record = home.path(&format!(".paseo/agents/work/{id}.json"));
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(
        record,
        json!({"id": id, "provider": provider, "cwd": home.project(), "persistence": {"provider": provider, "nativeHandle": handle}}).to_string(),
    )
    .unwrap();
}

// Pi keeps a directory per project there, so a file breaks reading its whole store
fn break_pi(home: &Home) {
    let sessions = home.path(".pi/agent/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("stray"), "").unwrap();
}

#[test]
fn an_import_keeps_what_it_could_read_and_still_fails() {
    let home = Home::new();
    claude_session(&home, CLAUDE_ID);
    break_pi(&home);
    assert!(!home.run(&["import"]).status.success());
    assert_eq!(home.sessions(&[]).len(), 1);
}

#[test]
fn importing_a_missing_path_fails() {
    let home = Home::new();
    let output = home.run(&["import", "--agent", "claude", "--path", "missing.jsonl"]);
    assert!(!output.status.success());
}

#[test]
fn materializing_into_rikkahub_needs_a_readable_backup_before_writing_anything() {
    let home = Home::new();
    pi_session(&home);
    let without_backup = home.run(&["materialize", "--in", "claude", "--in", "rikkahub"]);
    assert!(!without_backup.status.success());
    assert!(!home.path(".claude").exists());
    let backup = home.path("backup.zip");
    fs::write(&backup, "not a backup").unwrap();
    let broken_backup = home.run(&[
        "materialize",
        "--in",
        "rikkahub",
        "--backup",
        backup.to_str().unwrap(),
    ]);
    assert!(!broken_backup.status.success());
}

#[test]
fn a_paseo_import_links_the_agents_it_can_and_still_fails() {
    let home = Home::new();
    claude_session(&home, CLAUDE_ID);
    break_pi(&home);
    let pi_handle = home.path("pi-session.jsonl");
    paseo_agent(&home, "a-pi", "pi", pi_handle.to_str().unwrap());
    paseo_agent(&home, "b-claude", "claude", CLAUDE_ID);
    assert!(!home.run(&["import", "--agent", "paseo"]).status.success());
    assert_eq!(home.sessions(&["--agent", "paseo"]).len(), 1);
}

#[test]
fn paseo_references_resolve_exact_ids_and_unique_prefixes_only() {
    let home = Home::new();
    claude_session(&home, CLAUDE_ID);
    paseo_agent(&home, "paseo-1", "claude", CLAUDE_ID);
    home.baton(&["show", "paseo:paseo-1"]);
    let linked = home.sessions(&["--agent", "paseo"]);
    assert_eq!(linked.len(), 1);
    let other = "22222222-2222-4222-8222-222222222222";
    claude_session(&home, other);
    paseo_agent(&home, "paseo-10", "claude", other);
    assert!(!home.run(&["show", "paseo:paseo"]).status.success());
    assert_eq!(home.sessions(&[]), linked);
    assert!(home.baton(&["show", "paseo:paseo-1"]).contains(&linked[0]));
}

#[test]
fn resuming_in_paseo_without_its_daemon_exports_nothing() {
    let home = Home::new();
    home.install_paseo(false);
    let session = pi_session(&home);
    let records = home.path(&format!("baton/sessions/{session}/records"));
    let before = fs::read_dir(&records).unwrap().count();
    let output = home.run(&["resume", &session, "--in", "paseo", "--provider", "claude"]);
    assert!(!output.status.success());
    assert!(!home.path(".claude").exists());
    assert_eq!(fs::read_dir(records).unwrap().count(), before);
}

#[test]
fn materializing_paseo_keeps_the_registration_of_each_provider() {
    let home = Home::new();
    home.install_paseo(true);
    let session = pi_session(&home);
    let registered = [
        home.export_to_paseo(&session, "claude"),
        home.export_to_paseo(&session, "codex"),
    ];
    home.baton(&["materialize", "--in", "paseo"]);
    assert_eq!(home.registrations(), registered);
}

#[test]
fn a_paseo_agent_keeps_working_until_its_replacement_is_done() {
    let home = Home::new();
    home.install_paseo(true);
    let session = pi_session(&home);
    let old = home.export_to_paseo(&session, "claude");
    let transcript = home.claude_transcripts().remove(0);
    answer_in_pi(&home);
    fs::write(home.path("paseo-import-fails"), "").unwrap();
    home.baton(&["materialize", "--in", "paseo"]);
    assert_eq!(home.registrations(), std::slice::from_ref(&old));
    assert!(
        fs::read_to_string(&transcript)
            .unwrap()
            .contains("question")
    );
    fs::remove_file(home.path("paseo-import-fails")).unwrap();
    fs::write(home.path("paseo-delete-fails"), "").unwrap();
    home.baton(&["materialize", "--in", "paseo"]);
    let registrations = home.registrations();
    assert_eq!(registrations.len(), 2);
    assert!(registrations.contains(&old));
    assert!(
        fs::read_to_string(&transcript)
            .unwrap()
            .contains("question")
    );
    fs::remove_file(home.path("paseo-delete-fails")).unwrap();
    home.baton(&["materialize", "--in", "paseo"]);
    let registrations = home.registrations();
    assert_eq!(registrations.len(), 1);
    assert!(!registrations.contains(&old));
    assert!(!transcript.exists());
}

#[test]
fn materializing_a_provider_keeps_the_sessions_paseo_agents_open() {
    let home = Home::new();
    home.install_paseo(true);
    let session = pi_session(&home);
    home.export_to_paseo(&session, "claude");
    let transcript = home.claude_transcripts().remove(0);
    answer_in_pi(&home);
    home.baton(&["materialize", "--in", "claude"]);
    assert!(transcript.exists());
    assert_eq!(home.claude_transcripts().len(), 2);
}

#[test]
fn a_paseo_registration_continued_into_a_fork_survives_materializing() {
    let home = Home::new();
    home.install_paseo(true);
    let session = pi_session(&home);
    let registration = home.export_to_paseo(&session, "claude");
    answer_in_pi(&home);
    continue_claude(&home);
    home.baton(&["materialize", "--in", "paseo"]);
    assert_eq!(home.sessions(&[]).len(), 2);
    assert_eq!(home.registrations(), [registration]);
}
