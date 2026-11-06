// Editors and their agents record workspaces as `file://` URIs, with a slash before a Windows drive letter

pub(crate) fn from_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub(crate) fn path(uri: &str) -> Option<String> {
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let escaped = encoded
            .get(index + 1..index + 3)
            .filter(|_| encoded[index] == b'%')
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                bytes.push(byte);
                index += 3;
            }
            None => {
                bytes.push(encoded[index]);
                index += 1;
            }
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    let drive = path
        .as_bytes()
        .get(1..3)
        .is_some_and(|drive| drive[0].is_ascii_alphabetic() && drive[1] == b':');
    Some(if drive { path[1..].to_owned() } else { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_survive_their_uris() {
        assert_eq!(from_path("/home/me/my proj"), "file:///home/me/my%20proj");
        assert_eq!(from_path(r"C:\Users\me"), "file:///C:/Users/me");
        for path in ["/home/me/my proj", "/tmp/项目", "C:/Users/me"] {
            assert_eq!(super::path(&from_path(path)).as_deref(), Some(path));
        }
        assert_eq!(super::path("/not/a/uri"), None);
    }
}
