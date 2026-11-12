// Editor chats from before the agent graph keep each message as a JSON bubble, in the order their composer lists them

use anyhow::Result;
use baton_core::{Body, Draft, Thinking, ToolCall, ToolResult, portable_call_id};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::support::json;
use crate::support::normalize::{self, Content};

const HUMAN: i64 = 1;
const AI: i64 = 2;

// The bubbles of the composer `composer`, whose JSON is `data`; a bubble without a time takes the one before it, or `created`
pub(in crate::cursor) fn read(
    connection: &Connection,
    composer: &str,
    data: &Value,
    created: &str,
) -> Result<Vec<Draft>> {
    let mut drafts = Vec::new();
    let mut clock = created.to_owned();
    let mut model = None;
    let headers = data["fullConversationHeadersOnly"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for header in headers {
        let Some(id) = header["bubbleId"].as_str() else {
            continue;
        };
        let bubble: Option<String> = connection
            .prepare_cached("SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1")?
            .query_row([format!("bubbleId:{composer}:{id}")], |row| row.get(0))
            .optional()?;
        let Some(bubble) = bubble.and_then(|bubble| serde_json::from_str::<Value>(&bubble).ok())
        else {
            continue;
        };
        clock = json::timestamp(
            &bubble["createdAt"],
            &json::timestamp(&header["createdAt"], &clock),
        );
        let kind = if header["type"].is_null() {
            &bubble["type"]
        } else {
            &header["type"]
        };
        let kind = kind.as_i64();
        let bodies = bodies(kind, &bubble, id);
        if kind == Some(AI) && !bodies.is_empty()
            // `default` is the model picker's choice, not a model
            && let Some(next) = bubble["modelInfo"]["modelName"]
                .as_str()
                .filter(|name| !name.is_empty() && *name != "default")
            && model.as_deref() != Some(next)
        {
            drafts.push(Draft {
                timestamp: clock.clone(),
                native_entry_id: Some(id.to_owned()),
                body: Body::Meta(json!({"type": "model", "model": next})),
            });
            model = Some(next.to_owned());
        }
        for body in bodies {
            drafts.push(Draft {
                timestamp: clock.clone(),
                native_entry_id: Some(id.to_owned()),
                body,
            });
        }
    }
    Ok(drafts)
}

fn bodies(kind: Option<i64>, bubble: &Value, bubble_id: &str) -> Vec<Body> {
    let text = bubble["text"].as_str().unwrap_or_default();
    match kind {
        Some(HUMAN) => normalize::user(Content {
            text: text.to_owned(),
            images: Vec::new(),
        })
        .into_iter()
        .collect(),
        Some(AI) => {
            let mut bodies = Vec::new();
            let thinking = &bubble["thinking"];
            if let Some(text) = json::string(&thinking["text"]) {
                bodies.push(Body::Thinking(Thinking {
                    text,
                    signature: json::string(&thinking["signature"]),
                }));
            }
            bodies.extend(normalize::assistant(text));
            let tool = &bubble["toolFormerData"];
            if tool.is_object() {
                bodies.extend(tool_use(tool, bubble_id));
            }
            bodies
        }
        _ => Vec::new(),
    }
}

// A tool the assistant used, with its result once there is one; arguments and results are often JSON in strings
fn tool_use(tool: &Value, bubble_id: &str) -> Vec<Body> {
    // A bubble holds both sides of an id-less tool invocation
    let id = tool["toolCallId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .unwrap_or(bubble_id);
    let call_id = portable_call_id(id);
    let arguments = if tool["params"].is_null() {
        &tool["rawArgs"]
    } else {
        &tool["params"]
    };
    let mut bodies = vec![Body::ToolCall(ToolCall {
        call_id: call_id.clone(),
        name: tool["name"].as_str().unwrap_or("unknown").to_owned(),
        input: normalize::arguments(arguments),
    })];
    let output = if tool["result"].is_null() {
        &tool["error"]
    } else {
        &tool["result"]
    };
    if !output.is_null() {
        let failed = !tool["error"].is_null()
            || matches!(
                tool["status"].as_str(),
                Some("error" | "failed" | "cancelled")
            );
        bodies.push(Body::ToolResult(ToolResult {
            call_id,
            text: json::to_text(output),
            images: Vec::new(),
            is_error: failed,
        }));
    }
    bodies
}

#[cfg(test)]
mod tests {
    use baton_core::Body;
    use rusqlite::{Connection, params};
    use serde_json::json;

    #[test]
    fn tools_without_ids_pair_with_the_result_in_their_own_bubble() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
            )
            .unwrap();
        let mut headers = Vec::new();
        for (bubble, name) in [("t1", "read_file"), ("t2", "list_dir")] {
            let value = json!({"type": 2, "toolFormerData": {"name": name, "status": "completed", "params": "{}", "result": format!("from {name}")}});
            connection
                .execute(
                    "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                    params![format!("bubbleId:composer:{bubble}"), value.to_string()],
                )
                .unwrap();
            headers.push(json!({"bubbleId": bubble, "type": 2}));
        }
        let data = json!({"fullConversationHeadersOnly": headers});
        let drafts =
            super::read(&connection, "composer", &data, "2026-01-01T00:00:00.000Z").unwrap();
        let ids: Vec<&str> = drafts
            .iter()
            .filter_map(|draft| match &draft.body {
                Body::ToolCall(call) => Some(call.call_id.as_str()),
                Body::ToolResult(result) => Some(result.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[2], ids[3]);
        assert_ne!(ids[0], ids[2]);
    }
}
