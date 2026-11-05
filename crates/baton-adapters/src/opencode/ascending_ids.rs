use chrono::Utc;
use uuid::Uuid;

const TIME_BITS: u64 = (1 << 48) - 1;
const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

// OpenCode sorts messages and parts by id, whose first 48 bits count milliseconds times 4096
#[derive(Default)]
pub(super) struct AscendingIds {
    last: u64,
}

impl AscendingIds {
    pub(super) fn next(&mut self, prefix: &str, millis: i64) -> String {
        let value = ((millis.max(0) as u64).wrapping_mul(0x1000) & TIME_BITS).max(self.last + 1);
        self.last = value;
        format!("{prefix}_{value:012x}{}", random(14))
    }
}

// Session ids count down so that the newest session sorts first
pub(super) fn session_id() -> String {
    let value = !(Utc::now().timestamp_millis() as u64).wrapping_mul(0x1000) & TIME_BITS;
    format!("ses_{value:012x}{}", random(14))
}

fn random(length: usize) -> String {
    let uuid = Uuid::new_v4();
    let mut text = String::with_capacity(length);
    for byte in &uuid.as_bytes()[..length] {
        text.push(char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_ascend_even_within_one_millisecond() {
        let mut ids = AscendingIds::default();
        let first = ids.next("msg", 1_793_844_000_000);
        let second = ids.next("msg", 1_793_844_000_000);
        let earlier = ids.next("msg", 1_000);
        assert!(
            first < second && second < earlier,
            "OpenCode orders messages by id, so ids must never go backwards"
        );
        assert_eq!(first.len(), "msg_".len() + 26);
        assert!(session_id().starts_with("ses_"));
    }
}
