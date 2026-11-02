//! UTC timestamps in the form GitHub writes them and schedules name their
//! slots with: `2026-11-02T09:00:00Z`.

use solos_agent_core::model::Ms;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

/// Whole seconds; the milliseconds are dropped.
pub fn format(ms: Ms) -> String {
    let at = i64::try_from(ms / 1_000)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok());
    let Some(at) = at else {
        return ms.to_string();
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z`, or `None` for anything else.
pub fn parse(text: &str) -> Option<Ms> {
    let (date, clock) = text.trim().split_once('T')?;
    let clock = clock.strip_suffix('Z')?;
    let (clock, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut date = date.split('-');
    let (year, month, day) = (date.next()?, date.next()?, date.next()?);
    let mut clock = clock.split(':');
    let (hour, minute, second) = (clock.next()?, clock.next()?, clock.next()?);
    if date.next().is_some() || clock.next().is_some() {
        return None;
    }
    let date = Date::from_calendar_date(
        year.parse().ok()?,
        Month::try_from(month.parse::<u8>().ok()?).ok()?,
        day.parse().ok()?,
    )
    .ok()?;
    let clock = Time::from_hms(
        hour.parse().ok()?,
        minute.parse().ok()?,
        second.parse().ok()?,
    )
    .ok()?;
    let seconds = PrimitiveDateTime::new(date, clock)
        .assume_utc()
        .unix_timestamp();
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: String = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(3)
        .collect();
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(millis.parse().ok()?)
}

#[cfg(test)]
mod tests {
    use super::{format, parse};

    #[test]
    fn timestamps_read_and_write_as_utc() {
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("2026-11-02T09:30:05Z"), Some(1_793_611_805_000));
        assert_eq!(parse("2026-11-02T09:30:05.25Z"), Some(1_793_611_805_250));
        assert_eq!(format(1_793_611_805_250), "2026-11-02T09:30:05Z");
        assert_eq!(parse(&format(1_793_611_805_000)), Some(1_793_611_805_000));
        for bad in [
            "2026-11-02 09:30:05Z",
            "2026-11-02T09:30:05",
            "2026-11-02T09:30:05+01:00",
            "2026-13-02T09:30:05Z",
            "2026-11-02T25:30:05Z",
            "2026-11-02T09:30:05.x1Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
