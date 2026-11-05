use serde_json::Value;
use sha2::Sha256;

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;

    base16ct::lower::encode_string(&Sha256::digest(bytes))
}

// Sorted keys at every level keep hashes independent of how a native format ordered its fields
pub(crate) fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let value = json!({"b": [{"z": 1, "a": "\""}], "a": null});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":null,"b":[{"a":"\"","z":1}]}"#
        );
    }
}
