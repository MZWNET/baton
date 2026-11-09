use std::fmt::Write;

pub(super) fn project_key(cwd: &str) -> String {
    let mut key = String::new();
    let mut separator = false;
    for unit in cwd.encode_utf16() {
        if matches!(unit, 0x2f | 0x5c | 0x3a) {
            if !separator {
                key.push('-');
            }
            separator = true;
        } else if let Ok(byte) = u8::try_from(unit)
            && (byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            key.push(char::from(byte));
            separator = false;
        } else {
            write!(key, "~{unit:04X}").expect("writing to a string cannot fail");
            separator = false;
        }
    }
    let key = key.trim_start_matches('-');
    let key = if key.is_empty() { "root" } else { key };
    format!("--{}--", &key[..key.len().min(251)])
}
