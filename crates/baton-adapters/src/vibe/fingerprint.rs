use serde_json::Value;

use crate::support::identifiers;

// Vibe appends to a transcript only while its last line hashes to the stored fingerprint, which it computes over Python's `json.dumps(message, sort_keys=True)`
pub(super) fn fingerprint(message: &Value) -> String {
    let mut text = String::new();
    write(message, &mut text);
    identifiers::sha256_hex(text.as_bytes())
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                string(key, out);
                out.push_str(": ");
                write(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::String(text) => string(text, out),
        scalar => out.push_str(&scalar.to_string()),
    }
}

// Python escapes everything outside printable ASCII by default, as UTF-16 units
fn string(text: &str, out: &mut String) {
    out.push('"');
    for char in text.chars() {
        match char {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(char),
            _ => {
                let mut units = [0; 2];
                for unit in char.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Digests CPython computes for the same messages
    #[test]
    fn fingerprints_match_python() {
        let user = json!({"role": "user", "content": "hi", "injected": false, "message_id": "00000000-0000-4000-8000-000000000001"});
        assert_eq!(
            fingerprint(&user),
            "fed13c029addbebf265295c221741b0f0370800efce960d9702426675a87d284"
        );
        let assistant = json!({"role": "assistant", "content": "caf\u{e9} \u{2028} \u{1F600} \u{7f} \u{1} \t\"\\/", "injected": false,
                               "message_id": "00000000-0000-4000-8000-000000000002",
                               "tool_calls": [{"id": "call_1", "index": 0, "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}, "type": "function"}]});
        assert_eq!(
            fingerprint(&assistant),
            "530b2c82cd8bd7e6c450ebe54b7f8e0f138e86cb8c81e593bac02318cefcf65c"
        );
        let tool = json!({"role": "tool", "content": "out", "injected": false, "name": "bash", "tool_call_id": "call_1",
                          "tool_result": {"output": {"baton": {"is_error": false}}, "cancelled": false}});
        assert_eq!(
            fingerprint(&tool),
            "7f29869b887fff61ea9ca704b662b1c8844cddfa6b4beb57d81875ff802c301e"
        );
    }
}
