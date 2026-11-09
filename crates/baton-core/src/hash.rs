use std::io::{self, Write};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    base16ct::lower::encode_string(&Sha256::digest(bytes))
}

// Sorted keys at every level keep hashes independent of how a native format ordered its fields
pub(crate) fn canonical_sha256(prefix: &[u8], value: &Value) -> String {
    let mut out = HashWriter(Sha256::new());
    out.0.update(prefix);
    write_canonical(value, &mut out).expect("JSON values serialize to a hash");

    base16ct::lower::encode_string(&out.0.finalize())
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_canonical(value: &Value, out: &mut HashWriter) -> io::Result<()> {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.write_all(b"{")?;
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                serde_json::to_writer(&mut *out, key)?;
                out.write_all(b":")?;
                write_canonical(item, out)?;
            }
            out.write_all(b"}")?;
        }
        Value::Array(items) => {
            out.write_all(b"[")?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                write_canonical(item, out)?;
            }
            out.write_all(b"]")?;
        }
        Value::String(text) => serde_json::to_writer(&mut *out, text)?,
        Value::Number(number) => serde_json::to_writer(&mut *out, number)?,
        Value::Null => out.write_all(b"null")?,
        Value::Bool(true) => out.write_all(b"true")?,
        Value::Bool(false) => out.write_all(b"false")?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_sha256_sorts_keys_recursively() {
        let value = json!({"b": [{"z": 1, "a": "\""}], "a": null});
        assert_eq!(
            canonical_sha256(b"", &value),
            sha256_hex(br#"{"a":null,"b":[{"a":"\"","z":1}]}"#)
        );
    }
}
