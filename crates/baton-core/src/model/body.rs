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
    /// Session facts such as model changes, as an object with a `type` field; never written back as events, but the models they name reach exports through [`Models`](crate::replay::Models)
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

/// Keeps nonempty ids of ASCII letters, digits, `_` and `-`, and replaces any other id with `call_` and 32 hex digits of its SHA-256, since replacing characters can make two ids equal
pub fn portable_call_id(id: &str) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        id.to_owned()
    } else {
        format!("call_{}", &crate::hash::sha256_hex(id.as_bytes())[..32])
    }
}

/// The portable id of an older baton call id written by replacing the `|` pi and omp put between OpenAI call and item ids
fn portable_legacy_call_id(id: &str) -> Option<String> {
    let (call_suffix, function) = id.strip_prefix("call_")?.split_once('_')?;
    let (prefix, suffix) = function.split_once('_')?;
    if call_suffix.is_empty()
        || !matches!(prefix, "fc" | "ctc")
        || suffix.is_empty()
        || !call_suffix.bytes().all(|c| c.is_ascii_alphanumeric())
        || !suffix.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }

    Some(portable_call_id(&format!("call_{call_suffix}|{function}")))
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
    pub(crate) fn replace_legacy_call_id(&mut self) -> bool {
        let id = match self {
            Body::ToolCall(call) => &mut call.call_id,
            Body::ToolResult(result) => &mut result.call_id,
            _ => return false,
        };
        if let Some(mapped) = portable_legacy_call_id(id) {
            *id = mapped;
            return true;
        }

        false
    }

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

#[cfg(test)]
mod tests {
    use super::{portable_call_id, portable_legacy_call_id};

    #[test]
    fn converts_only_legacy_openai_call_ids_to_portable_ids() {
        for (old, original) in [
            ("call_X_fc_Y", "call_X|fc_Y"),
            ("call_123_ctc_ABC", "call_123|ctc_ABC"),
        ] {
            assert_eq!(
                portable_legacy_call_id(old),
                Some(portable_call_id(original))
            );
        }
        for id in [
            "call__fc_Y",
            "call_X_fc_",
            "call_X_fc_Y_Z",
            "call_X_other_Y",
            "call_X-Y_fc_Z",
            "call_X|fc_Y",
            "call_function_abcdefghijkl_1",
        ] {
            assert_eq!(portable_legacy_call_id(id), None, "{id}");
        }
    }
}
