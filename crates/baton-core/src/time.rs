//! Timestamps as stored in events: RFC 3339 in UTC with millisecond precision.

use chrono::{DateTime, SecondsFormat, Utc};

fn format(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The current time, formatted the way events store timestamps.
pub fn now() -> String {
    format(Utc::now())
}

/// Parses an RFC 3339 timestamp in any offset.
pub fn parse(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Reformats an RFC 3339 timestamp the way events store timestamps.
pub fn normalize(value: &str) -> Option<String> {
    parse(value).map(format)
}

/// Formats milliseconds since the Unix epoch the way events store timestamps.
pub fn from_unix_millis(millis: i64) -> Option<String> {
    DateTime::from_timestamp_millis(millis).map(format)
}

/// Milliseconds since the Unix epoch of an RFC 3339 timestamp, or 0 when it does not parse.
pub fn unix_millis(value: &str) -> i64 {
    parse(value).map_or(0, |time| time.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_normalize_to_utc_milliseconds() {
        assert_eq!(
            normalize("2026-01-05T17:00:00.123456+08:00").as_deref(),
            Some("2026-01-05T09:00:00.123Z")
        );
        assert_eq!(
            from_unix_millis(1_787_659_200_000).as_deref(),
            Some("2026-08-25T12:00:00.000Z")
        );
        assert_eq!(unix_millis("2026-08-25T12:00:00Z"), 1_787_659_200_000);
        assert_eq!(normalize("yesterday"), None);
    }
}
