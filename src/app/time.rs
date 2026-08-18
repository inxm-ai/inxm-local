//! Time-zone conversion helpers for user-facing timestamps.
//!
//! Persisted timestamps remain UTC. Convert them only at presentation
//! boundaries so the UI follows the operating system's configured time zone.

use std::fmt::Display;

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};

pub fn format_local(timestamp: &DateTime<Utc>, format: &str) -> String {
    format_in_timezone(timestamp, &Local, format)
}

pub fn local_date(timestamp: &DateTime<Utc>) -> NaiveDate {
    date_in_timezone(timestamp, &Local)
}

fn format_in_timezone<Tz>(timestamp: &DateTime<Utc>, timezone: &Tz, format: &str) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    timestamp.with_timezone(timezone).format(format).to_string()
}

fn date_in_timezone<Tz>(timestamp: &DateTime<Utc>, timezone: &Tz) -> NaiveDate
where
    Tz: TimeZone,
{
    timestamp.with_timezone(timezone).date_naive()
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone, Utc};

    use super::{date_in_timezone, format_in_timezone};

    #[test]
    fn formatting_converts_utc_to_the_display_timezone() {
        let timestamp = Utc.with_ymd_and_hms(2026, 8, 10, 10, 15, 0).unwrap();
        let utc_plus_two = FixedOffset::east_opt(2 * 60 * 60).unwrap();

        assert_eq!(
            format_in_timezone(&timestamp, &utc_plus_two, "%b %d %H:%M"),
            "Aug 10 12:15"
        );
    }

    #[test]
    fn display_date_accounts_for_local_day_rollover() {
        let timestamp = Utc.with_ymd_and_hms(2026, 8, 10, 23, 30, 0).unwrap();
        let utc_plus_two = FixedOffset::east_opt(2 * 60 * 60).unwrap();

        assert_eq!(
            date_in_timezone(&timestamp, &utc_plus_two),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 11).unwrap()
        );
    }
}
