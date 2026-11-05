#![allow(dead_code)]

use std::fs;
use std::path::Path;

use baton_core::{Assistant, Body, Compaction, Draft, Image, Thinking, ToolCall, ToolResult, User};
use serde_json::Value;

pub fn jsonl(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn draft(body: Body) -> Draft {
    Draft {
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        native_entry_id: None,
        body,
    }
}

pub fn bodies(drafts: &[Draft]) -> Vec<Body> {
    drafts.iter().map(|draft| draft.body.clone()).collect()
}

pub fn user(text: &str, images: Vec<Image>) -> Body {
    Body::User(User {
        text: text.into(),
        images,
    })
}

pub fn assistant(text: &str) -> Body {
    Body::Assistant(Assistant { text: text.into() })
}

pub fn thinking(text: &str, signature: Option<&str>) -> Body {
    Body::Thinking(Thinking {
        text: text.into(),
        signature: signature.map(String::from),
    })
}

pub fn call(id: &str, name: &str, input: Value) -> Body {
    Body::ToolCall(ToolCall {
        call_id: id.into(),
        name: name.into(),
        input,
    })
}

pub fn result(id: &str, text: &str, images: Vec<Image>, is_error: bool) -> Body {
    Body::ToolResult(ToolResult {
        call_id: id.into(),
        text: text.into(),
        images,
        is_error,
    })
}

pub fn compaction(summary: &str) -> Body {
    Body::Compaction(Compaction {
        summary: summary.into(),
    })
}

pub fn meta(value: Value) -> Body {
    Body::Meta(value)
}
