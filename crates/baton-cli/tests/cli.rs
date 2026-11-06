//! End-to-end runs of the `baton` binary against isolated agent stores: incremental imports, backflow, forks and search.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Home {
        let home = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(home.project()).unwrap();
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn project(&self) -> PathBuf {
        self.path("project")
    }

    // Agents keep their stores under HOME on Unix and USERPROFILE on Windows, unless these variables move them
    const STORE_VARIABLES: [&str; 22] = [
        "PI_CODING_AGENT_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "HERMES_HOME",
        "MASTRA_DB_PATH",
        "MASTRA_APP_DATA_DIR",
        "MASTRA_RESOURCE_ID",
        "DEVIN_SESSIONS_DIR",
        "COPILOT_HOME",
        "GROK_HOME",
        "OPENHANDS_CONVERSATIONS_DIR",
        "OPENHANDS_PERSISTENCE_DIR",
        "VIBE_HOME",
        "VIBE_SESSION_LOGGING__SAVE_DIR",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
        "KIMI_CODE_HOME",
        "CURSOR_CONFIG_DIR",
    ];

    fn baton(&self, args: &[&str]) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_baton"));
        command
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .env("BATON_HOME", self.path("baton"))
            .env("BATON_CACHE_DIR", self.path("cache"));
        for variable in Home::STORE_VARIABLES {
            command.env_remove(variable);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "baton {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn sessions(&self) -> Vec<String> {
        self.baton(&["list"])
            .lines()
            .map(|line| line.split_whitespace().next().unwrap().to_owned())
            .collect()
    }

    fn event_count(&self, session: &str) -> usize {
        fs::read_to_string(self.path(&format!("baton/sessions/{session}/events.jsonl")))
            .unwrap()
            .lines()
            .count()
    }

    fn pi_file(&self) -> PathBuf {
        self.path(".pi/agent/sessions/--work--/2026-01-01T00-00-00-000Z_pi-1.jsonl")
    }

    // Exports into the project directory, where baton runs, and returns the native id
    fn export(&self, session: &str, agent: &str) -> String {
        self.baton(&["export", session, "--to", agent, "--cwd", "."])
            .trim()
            .to_owned()
    }

    // Exports only ever go into the project directory, so Claude Code has a single project
    fn claude_transcript(&self, id: &str) -> PathBuf {
        let project = fs::read_dir(self.path(".claude/projects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        project.join(format!("{id}.jsonl"))
    }
}

fn jsonl(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

fn append(path: &Path, records: &[Value]) {
    use std::io::Write;

    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(jsonl(records).as_bytes()).unwrap();
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
    assert!(home.baton(&["import"]).contains("1 new"));
    assert!(
        home.path("cache/state.db").is_file(),
        "cursors live in BATON_CACHE_DIR"
    );
    assert!(
        home.baton(&["import"])
            .contains("0 new, 0 updated, 0 forked, 1 unchanged")
    );
    let session = home.sessions().remove(0);

    append(
        &home.pi_file(),
        &[
            ask("a3", Some("a2"), 3, "second question"),
            answer("a4", "a3", 4, "second answer"),
        ],
    );
    let output = home.baton(&["import"]);
    assert!(
        output.contains(&format!("update  {session}  +2")),
        "{output}"
    );
    assert_eq!(home.event_count(&session), 4);

    append(
        &home.pi_file(),
        &[ask("b1", Some("a2"), 5, "branch question")],
    );
    assert!(home.baton(&["import"]).contains("+1"));
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
    assert!(home.baton(&["import"]).contains("0 updated"));
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
    let session = home.sessions().remove(0);
    let claude_id = home.export(&session, "claude");
    let output = home.baton(&["import"]);
    assert!(output.contains("0 new, 0 updated, 0 forked"), "{output}");

    let transcript = home.claude_transcript(&claude_id);
    let last = fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Value>(line).ok()?["uuid"]
                .as_str()
                .map(String::from)
        })
        .next_back()
        .unwrap();
    let project = home.project();
    let record = |uuid: &str, parent: &str, kind: &str, content: Value| {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": claude_id, "isSidechain": false,
               "cwd": project, "timestamp": "2026-01-02T00:00:00.000Z", "message": {"role": kind, "content": content}})
    };
    append(
        &transcript,
        &[
            record("c1", &last, "user", json!("question two")),
            record(
                "c2",
                "c1",
                "assistant",
                json!([{"type": "text", "text": "answer two"}]),
            ),
        ],
    );
    let output = home.baton(&["import", "--agent", "claude"]);
    assert!(
        output.contains(&format!("update  {session}  +2")),
        "{output}"
    );
    assert!(home.baton(&["show", &session]).contains("answer two"));

    append(
        &home.pi_file(),
        &[ask("a3", Some("a2"), 3, "pi only question")],
    );
    let output = home.baton(&["import", "--agent", "pi"]);
    let fork = output
        .lines()
        .find_map(|line| line.strip_prefix("fork    "))
        .unwrap_or_else(|| panic!("expected a fork: {output}"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let shown = home.baton(&["show", &fork]);
    assert!(
        shown.contains(&format!("forked   from {session}")),
        "{shown}"
    );
    assert!(
        shown.contains("pi only question")
            && shown.contains("answer one")
            && !shown.contains("answer two")
    );
    assert!(
        home.baton(&["import"])
            .contains("0 new, 0 updated, 0 forked")
    );
    assert!(
        home.baton(&["show", &format!("claude:{}", &claude_id[..8])])
            .contains(&session)
    );
}

#[test]
fn exports_record_the_absolute_project_directory() {
    let home = Home::new();
    write_pi(&home, &[header("/work"), ask("a1", None, 1, "question")]);
    home.baton(&["import"]);
    let session = home.sessions().remove(0);
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
    let session = home.sessions().remove(0);
    let claude_id = home.export(&session, "claude");
    fs::write(home.claude_transcript(&claude_id), "").unwrap();
    let output = home.baton(&["import"]);
    assert!(
        output.contains("0 new, 0 updated, 0 forked, 2 unchanged"),
        "{output}"
    );
    assert_eq!(home.event_count(&session), 2);
}

#[test]
fn every_file_agent_reads_its_own_export_back_unchanged() {
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
    let session = home.sessions().remove(0);
    for agent in ["pi", "omp", "claude", "codex"] {
        home.export(&session, agent);
    }
    let output = home.baton(&["import"]);
    assert!(
        output.contains("0 new, 0 updated, 0 forked, 5 unchanged"),
        "{output}"
    );
    let listed = home.baton(&["list", "--agent", "codex"]);
    assert!(
        listed.contains(&session) && listed.contains("Round trip"),
        "{listed}"
    );
}

#[test]
fn sessions_are_searchable_and_resolvable_by_prefix() {
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
    let session = home.sessions().remove(0);
    assert!(home.baton(&["search", "配置文件"]).contains(&session));
    assert!(home.baton(&["search", "WHERE", "the"]).contains(&session));
    assert!(
        home.baton(&["search", "配置", "where"]).contains(&session),
        "terms shorter than a trigram are still found"
    );
    assert!(home.baton(&["search", "nowhere"]).is_empty());
    assert!(
        home.baton(&["search", "%"]).is_empty(),
        "a wildcard character in a term only matches itself"
    );
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
