use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use baton_core::{
    Agent, Blobs, Body, Cursor, Draft, Info, NativeRead, NativeSession, Progress, Source, Stamp,
    Thinking, ToolCall, ToolResult, journal, time,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use uuid::Uuid;

use crate::antigravity::summary::{self, Summary};
use crate::support::protobuf::Message;
use crate::support::{files, images, json, normalize, sqlite};

// The editor state key holding the trajectory summaries
const SUMMARIES_KEY: &str = "antigravityUnifiedStateSync.trajectorySummaries";

/// Conversations of the [Antigravity](https://antigravity.google/) IDE: the transcript logs under `~/.gemini/antigravity-ide/brain`, with titles and workspaces in the editor's state database.
///
/// baton reads them but cannot write them: the logs are derived output and no command opens a given conversation. The logs carry no call ids, so results answer the oldest call still waiting.
pub struct AntigravityIdeAdapter {
    root: PathBuf,
    user_data: PathBuf,
}

impl AntigravityIdeAdapter {
    /// An adapter for the IDE's data directory `root` and the editor's user data directory `user_data`.
    pub fn new(root: PathBuf, user_data: PathBuf) -> AntigravityIdeAdapter {
        AntigravityIdeAdapter { root, user_data }
    }

    /// The data directory, `~/.gemini/antigravity-ide`.
    pub fn default_root() -> Result<PathBuf> {
        Ok(files::home()?.join(".gemini").join("antigravity-ide"))
    }

    /// The editor's user data directory, `Antigravity` in the platform's application data or configuration directory.
    pub fn default_user_data() -> Result<PathBuf> {
        files::editor_data("Antigravity")
    }

    // A missing or unreadable editor state only costs the titles
    fn summaries(&self) -> Result<HashMap<String, Summary>> {
        let db = self
            .user_data
            .join("User")
            .join("globalStorage")
            .join("state.vscdb");
        if !db.is_file() {
            return Ok(HashMap::new());
        }
        let connection = sqlite::open_read_only(&db)?;
        let topic: Option<String> = connection
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [SUMMARIES_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let topic = topic
            .and_then(|topic| images::decode(&topic))
            .unwrap_or_default();
        let mut summaries = HashMap::new();
        for (id, row) in summary::entries(&topic) {
            let found = Message::parse(&row)
                .ok()
                .and_then(|row| images::decode(row.string(1)?))
                .and_then(|bytes| summary::summary(&bytes));
            if let Some(found) = found {
                summaries.insert(id, found);
            }
        }
        Ok(summaries)
    }
}

// The untruncated log when the IDE wrote one
fn transcript(dir: &Path) -> Option<PathBuf> {
    let logs = dir.join(".system_generated").join("logs");
    ["transcript_full.jsonl", "transcript.jsonl"]
        .iter()
        .map(|name| logs.join(name))
        .find(|log| log.is_file())
}

impl Source for AntigravityIdeAdapter {
    fn agent(&self) -> Agent {
        Agent::AntigravityIde
    }

    fn discover(&self, path: Option<&Path>) -> Result<Vec<NativeSession>> {
        let root = path.unwrap_or(&self.root);
        let summaries = self.summaries()?;
        let mut sessions = Vec::new();
        for dir in files::read_dir(&root.join("brain"))? {
            let Some(id) = dir
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| Uuid::parse_str(name).is_ok())
            else {
                continue;
            };
            let Some(log) = transcript(&dir) else {
                continue;
            };
            sessions.push(NativeSession {
                id: id.to_owned(),
                stamp: Stamp::of(&log)?,
                title: summaries.get(id).and_then(|summary| summary.title.clone()),
                path: log,
            });
        }
        Ok(sessions)
    }

    fn read(
        &self,
        session: &NativeSession,
        _cursor: Option<&Cursor>,
        _blobs: &Blobs,
    ) -> Result<NativeRead> {
        let bytes = fs::read(&session.path)
            .with_context(|| format!("reading {}", session.path.display()))?;
        let mut reader = LogReader::default();
        for line in journal::lines(&bytes) {
            if let Ok(record) = serde_json::from_slice::<Value>(line.bytes) {
                reader.record(line.start, &record);
            }
        }
        let summaries = self.summaries()?;
        let summary = summaries.get(&session.id);
        Ok(NativeRead {
            info: Info {
                cwd: summary
                    .and_then(|summary| summary.cwd.clone())
                    .or(reader.workspace.flatten()),
                title: summary.and_then(|summary| summary.title.clone()),
                created: summary
                    .and_then(|summary| summary.created.clone())
                    .or(reader.created),
            },
            drafts: reader.drafts,
            continued: false,
            progress: Progress::default(),
        })
    }
}

#[derive(Default)]
struct LogReader {
    drafts: Vec<Draft>,
    pending: VecDeque<String>,
    created: Option<String>,
    // The single directory every record names as its workspace, `Some(None)` once they disagree
    workspace: Option<Option<String>>,
}

impl LogReader {
    fn record(&mut self, offset: usize, record: &Value) {
        let timestamp =
            record_time(record).unwrap_or_else(|| self.created.clone().unwrap_or_else(time::now));
        if self.created.is_none() {
            self.created = Some(timestamp.clone());
        }
        if let Some(paths) = record["workspacePaths"].as_array() {
            let single = match paths.as_slice() {
                [path] => path.as_str().map(String::from),
                _ => None,
            };
            self.workspace = match &self.workspace {
                None => Some(single),
                Some(known) if *known == single => Some(single),
                Some(_) => Some(None),
            };
        }
        let kind = record["type"].as_str().unwrap_or_default();
        let kind = kind.strip_prefix("CORTEX_STEP_TYPE_").unwrap_or(kind);
        let bodies = match kind {
            "USER_INPUT" => {
                let input = &record["userInput"];
                let text = [&record["content"], &input["query"], &input["userResponse"]]
                    .into_iter()
                    .find_map(json::string)
                    .unwrap_or_default();
                // The log wraps what the user typed in a request tag, followed by metadata the IDE added
                let text = match text.strip_prefix("<USER_REQUEST>") {
                    Some(rest) => rest
                        .split("</USER_REQUEST>")
                        .next()
                        .unwrap_or(rest)
                        .trim()
                        .to_owned(),
                    None => text,
                };
                normalize::user(normalize::Content {
                    text,
                    images: Vec::new(),
                })
                .into_iter()
                .collect()
            }
            "PLANNER_RESPONSE" => {
                let mut bodies = Vec::new();
                if let Some(text) = json::string(&record["thinking"]) {
                    bodies.push(Body::Thinking(Thinking {
                        text,
                        signature: None,
                    }));
                }
                let planner = &record["plannerResponse"];
                let text = [
                    &record["content"],
                    &planner["modifiedResponse"],
                    &planner["response"],
                ]
                .into_iter()
                .find_map(json::string);
                bodies.extend(text.as_deref().and_then(normalize::assistant));
                let step = record["step_index"].as_i64().unwrap_or(offset as i64);
                let calls = record["tool_calls"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for (index, call) in calls.iter().enumerate() {
                    let id = format!("step{step}_{index}");
                    self.pending.push_back(id.clone());
                    bodies.push(Body::ToolCall(ToolCall {
                        call_id: id,
                        name: call["name"].as_str().unwrap_or("unknown").to_owned(),
                        input: normalize::tool_input(call["args"].clone()),
                    }));
                }
                bodies
            }
            "CHECKPOINT" | "SYSTEM_MESSAGE" | "CONVERSATION_HISTORY" | "" => Vec::new(),
            // Any other step with output while a call waits is that call's result
            _ => match (json::string(&record["content"]), self.pending.is_empty()) {
                (Some(text), false) => {
                    let call = self.pending.pop_front().expect("a call is waiting");
                    vec![Body::ToolResult(ToolResult {
                        call_id: call,
                        text,
                        images: Vec::new(),
                        is_error: false,
                    })]
                }
                _ => Vec::new(),
            },
        };
        for body in bodies {
            self.drafts.push(Draft {
                timestamp: timestamp.clone(),
                native_entry_id: Some(offset.to_string()),
                body,
            });
        }
    }
}

// Records give their time as RFC 3339 text or as epoch milliseconds or seconds
fn record_time(record: &Value) -> Option<String> {
    let value = if record["created_at"].is_null() {
        &record["metadata"]["createdAt"]
    } else {
        &record["created_at"]
    };
    match value {
        Value::String(text) => time::normalize(text),
        Value::Number(number) => {
            let number = number.as_f64()?;
            let millis = if number.abs() >= 1e11 {
                number
            } else {
                number * 1000.0
            };
            time::from_unix_millis(millis as i64)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn record_times_take_every_encoding() {
        assert_eq!(
            record_time(&json!({"created_at": "2026-01-01T00:00:00Z"})).as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            record_time(&json!({"created_at": 1_767_225_600_000_i64})).as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            record_time(&json!({"metadata": {"createdAt": 1_767_225_600}})).as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
    }
}
