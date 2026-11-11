// Agents that stamp entries with the local time of the machine they ran on and no offset, which baton reads and writes in the zone TZ names, or else the system's

use anyhow::{Context, Result};
use baton_core::time;
use chrono::{DateTime, Datelike, NaiveDateTime, TimeDelta, Timelike};
use jiff::tz::TimeZone;
use jiff::{Timestamp, civil};

// TZ names the zone on every platform, and one that names none is refused rather than replaced
fn zone() -> Result<TimeZone> {
    TimeZone::try_system()
        .context("finding the time zone of local times; set TZ to it, such as TZ=Asia/Shanghai")
}

pub(crate) fn naive(timestamp: &str) -> Result<NaiveDateTime> {
    let millis = time::unix_millis(timestamp);
    let offset = zone()?.to_offset(Timestamp::from_millisecond(millis)?);
    let utc = DateTime::from_timestamp_millis(millis)
        .expect("chrono covers every instant jiff does")
        .naive_utc();

    Ok(utc + TimeDelta::seconds(offset.seconds().into()))
}

// The hour daylight saving time skips names no instant and the hour it repeats names two, so their local times are refused rather than guessed
pub(crate) fn instant(local: NaiveDateTime) -> Result<String> {
    let civil = civil::DateTime::new(
        i16::try_from(local.year())?,
        local.month() as i8,
        local.day() as i8,
        local.hour() as i8,
        local.minute() as i8,
        local.second() as i8,
        local.nanosecond() as i32,
    )?;
    let instant = zone()?
        .to_ambiguous_timestamp(civil)
        .unambiguous()
        .with_context(|| {
            format!(
                "the local time {local} names no single instant; set TZ to the fixed offset it was recorded at"
            )
        })?;

    Ok(time::from_unix_millis(instant.as_millisecond()).expect("jiff instants fit in chrono"))
}

// kotlinx-datetime leaves the seconds out of round minutes
pub(crate) fn parse(value: &str) -> Result<String> {
    let local = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .with_context(|| format!("reading the local time {value:?}"))?;

    instant(local)
}
