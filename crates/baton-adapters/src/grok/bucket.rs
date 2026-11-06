use crate::support::identifiers;

// Grok names the directory of a working directory's sessions after the percent-encoded path
pub(super) fn name(cwd: &str) -> String {
    let mut encoded = String::with_capacity(cwd.len());
    for byte in cwd.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

// For a path too long to encode in a file name Grok uses a slug and a BLAKE3 hash, recording the path in `.cwd`; baton hashes with SHA-256 instead, which Grok still finds by session id through that file
pub(super) fn hashed(cwd: &str) -> String {
    let last = cwd
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    let mut slug = String::new();
    for char in last.chars() {
        if char.is_ascii_alphanumeric() {
            slug.push(char);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = if slug.is_empty() {
        "workspace".into()
    } else {
        slug
    };
    format!("{slug}-{}", &identifiers::sha256_hex(cwd.as_bytes())[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_follow_grok_naming() {
        assert_eq!(name("/tmp/a b"), "%2Ftmp%2Fa%20b");
        assert_eq!(name(r"C:\w"), "C%3A%5Cw");
        let long = format!("/work/{}/项目", "深".repeat(40));
        assert!(name(&long).len() > 255);
        assert!(hashed(&long).starts_with("workspace-"));
        assert!(hashed("/work/My Repo").starts_with("my-repo-"));
    }
}
