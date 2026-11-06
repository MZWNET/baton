// Editor chats from before the agent graph keep each message as a JSON bubble, in the order their composer lists them

use anyhow::Result;
use baton_core::{Body, Draft, Thinking, ToolCall, ToolResult};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::support::json;
use crate::support::normalize::{self, Content};

const HUMAN: i64 = 1;
const AI: i64 = 2;

// The bubbles of the composer `composer`, whose JSON is `data`; a bubble without a time takes the one before it, or `created`
pub(super) fn read(
    connection: &Connection,
    composer: &str,
    data: &Value,
    created: &str,
) -> Result<Vec<Draft>> {
    let mut drafts = Vec::new();
    let mut clock = created.to_owned();
    let headers = data["fullConversationHeadersOnly"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for header in headers {
        let Some(id) = header["bubbleId"].as_str() else {
            continue;
        };
        let bubble: Option<String> = connection
            .query_row(
                "SELECT CAST(value AS TEXT) FROM cursorDiskKV WHERE key = ?1",
                [format!("bubbleId:{composer}:{id}")],
                |row| row.get(0),
            )
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
        for body in bodies(kind.as_i64(), &bubble) {
            drafts.push(Draft {
                timestamp: clock.clone(),
                native_entry_id: Some(id.to_owned()),
                body,
            });
        }
    }
    Ok(drafts)
}

fn bodies(kind: Option<i64>, bubble: &Value) -> Vec<Body> {
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
                bodies.extend(tool_use(tool));
            }
            bodies
        }
        _ => Vec::new(),
    }
}

// A tool the assistant used, with its result once there is one; arguments and results are often JSON in strings
fn tool_use(tool: &Value) -> Vec<Body> {
    let call_id = normalize::call_id(&tool["toolCallId"]);
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
