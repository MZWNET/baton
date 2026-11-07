use uuid::Uuid;

pub(crate) fn session_v4(seed: Option<Uuid>) -> Uuid {
    seed.map_or_else(Uuid::new_v4, |seed| {
        uuid::Builder::from_random_bytes(seed.into_bytes()).into_uuid()
    })
}

// Time ordered, like the session ids pi, Oh My Pi and Codex generate themselves
pub(crate) fn session_v7(seed: Option<Uuid>, created_millis: u64) -> Uuid {
    seed.map_or_else(Uuid::now_v7, |seed| {
        uuid::Builder::from_unix_timestamp_millis(
            created_millis,
            seed.as_bytes()[..10].try_into().unwrap(),
        )
        .into_uuid()
    })
}

pub(crate) fn uuid_v4() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn short_hex(length: usize) -> String {
    Uuid::new_v4().simple().to_string()[..length].to_owned()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;

    base16ct::lower::encode_string(&sha2::Sha256::digest(bytes))
}
