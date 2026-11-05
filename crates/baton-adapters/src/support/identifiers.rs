use uuid::Uuid;

pub(crate) fn uuid_v4() -> String {
    Uuid::new_v4().to_string()
}

// Time ordered, like the session ids pi, Oh My Pi and Codex generate themselves
pub(crate) fn uuid_v7() -> String {
    Uuid::now_v7().to_string()
}

pub(crate) fn short_hex(length: usize) -> String {
    Uuid::new_v4().simple().to_string()[..length].to_owned()
}
