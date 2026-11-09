use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What an event says, stored as its `kind` and `payload` fields.
///
/// Payloads keep only what every agent can represent, so that a session written to any agent reads back to the same bodies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Body {
    /// A message from the user.
    User(User),
    /// One block of text from the assistant.
    Assistant(Assistant),
    /// A tool call made by the assistant.
    ToolCall(ToolCall),
    /// The result of a tool call.
    ToolResult(ToolResult),
    /// A summary that replaces everything before it in the model's context.
    Compaction(Compaction),
    /// Reasoning of the assistant, kept but never written back to an agent.
    Thinking(Thinking),
    /// Session facts such as model changes, as an object with a `type` field; never written back to an agent.
    ///
    /// A meta event of type `reset` hides everything before it from exports, the way an Oh My Pi reset boundary does.
    Meta(Value),
}

/// An image stored in [`Blobs`](crate::Blobs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// The SHA-256 the image is stored under.
    pub blob: String,
    /// The media type, such as `image/png`.
    pub mime: String,
}

/// A message from the user, with its text blocks joined by newlines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// The text of the message.
    pub text: String,
    /// Images attached to the message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

/// One block of text from the assistant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    /// The text of the block.
    pub text: String,
}

/// A tool call made by the assistant, kept as history and never run again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The call id, limited to ASCII letters, digits, `_` and `-` so every provider accepts it.
    pub call_id: String,
    /// The tool name.
    pub name: String,
    /// The arguments, always a JSON object; other values are wrapped as `{"input": value}`.
    pub input: Value,
}

/// The result of a tool call, with its text blocks joined by newlines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The id of the call this result answers.
    pub call_id: String,
    /// The text output.
    pub text: String,
    /// Images in the output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    /// Whether the tool failed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// A summary that replaces everything before it in the model's context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// The summary text.
    pub summary: String,
}

/// Reasoning of the assistant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thinking {
    /// The readable reasoning, empty when the provider only returned it encrypted.
    pub text: String,
    /// The provider's signature or encrypted reasoning, which only that provider can verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl Body {
    /// The `kind` the body is stored under, such as `tool_call`.
    pub fn kind(&self) -> &'static str {
        match self {
            Body::User(_) => "user",
            Body::Assistant(_) => "assistant",
            Body::ToolCall(_) => "tool_call",
            Body::ToolResult(_) => "tool_result",
            Body::Compaction(_) => "compaction",
            Body::Thinking(_) => "thinking",
            Body::Meta(_) => "meta",
        }
    }

    pub(crate) fn replayable(&self) -> bool {
        !matches!(self, Body::Thinking(_) | Body::Meta(_))
    }

    pub(crate) fn is_reset(&self) -> bool {
        matches!(self, Body::Meta(meta) if meta["type"] == "reset")
    }

    /// The text of the body: message text, the tool name with its arguments, tool output, or the summary
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Body::User(user) => Cow::Borrowed(&user.text),
            Body::Assistant(assistant) => Cow::Borrowed(&assistant.text),
            Body::ToolCall(call) => Cow::Owned(format!("{} {}", call.name, call.input)),
            Body::ToolResult(result) => Cow::Borrowed(&result.text),
            Body::Compaction(compaction) => Cow::Borrowed(&compaction.summary),
            Body::Thinking(thinking) => Cow::Borrowed(&thinking.text),
            Body::Meta(meta) => Cow::Owned(meta.to_string()),
        }
    }
}
