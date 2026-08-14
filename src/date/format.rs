//! Formatting helpers for epoch timestamps and durations.

use crate::date::Epoch;
use jiff::civil::Weekday;

/// Format seconds as a human-readable duration string (e.g. "1 day", "2 hours").
pub fn format_duration(secs: i64) -> String {
    humantime::format_duration(std::time::Duration::from_secs(secs as u64)).to_string()
}

/// Format an epoch timestamp as `HH:MM`.
pub fn format_time(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .and_then(|z| jiff::fmt::strtime::format("%H:%M", &z).ok())
        .unwrap_or_else(|| "--:--".to_string())
}

/// Two-letter local weekday abbreviation for an epoch ("Mo".."Su").
pub fn format_weekday(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .map(|z| match z.weekday() {
            Weekday::Monday => "Mo",
            Weekday::Tuesday => "Tu",
            Weekday::Wednesday => "We",
            Weekday::Thursday => "Th",
            Weekday::Friday => "Fr",
            Weekday::Saturday => "Sa",
            Weekday::Sunday => "Su",
        })
        .unwrap_or_default()
        .to_string()
}

/// Format an epoch timestamp as `DD-MM-YY`.
pub fn format_date(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .and_then(|z| jiff::fmt::strtime::format("%d-%m-%y", &z).ok())
        .unwrap_or_else(|| "--".to_string())
}

/// Format an epoch timestamp as `YYYY-MM-DD HH:MM`.
pub fn format_datetime(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .and_then(|z| jiff::fmt::strtime::format("%Y-%m-%d %H:%M", &z).ok())
        .unwrap_or_else(|| "--".to_string())
}

/// Human-friendly datetime for preview field lines (`prev:`, `next:`,
/// `last:`, ...). Currently defers to [`format_datetime`]; a future
/// humanization (e.g. relative phrasing like "today 14:30") goes here
/// without touching the field renderers. The right-aligned dark-gray date
/// line keeps [`format_datetime`].
pub fn format_human_datetime(ts: Epoch) -> String {
    format_datetime(ts)
}

/// Short datetime form for per-entry annotations (e.g. the text-tracker
/// `> value [timestamp]` lines); M-D HH:MM (hour/minute zero-padded)
pub fn format_datetime_short(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .and_then(|z| jiff::fmt::strtime::format("%-m-%-d %H:%M", &z).ok())
        .unwrap_or_else(|| "--".to_string())
}

/// DD HH:MM
pub fn format_day_time(ts: Epoch) -> String {
    crate::date::zoned_from_unix_secs(ts)
        .ok()
        .and_then(|z| jiff::fmt::strtime::format("%d %H:%M", &z).ok())
        .unwrap_or_else(|| "--".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::parse;

    #[test]
    fn test_format_duration_roundtrip() {
        let secs = 86400;
        let s = format_duration(secs);
        assert_eq!(s, "1day");
    }

    #[test]
    fn test_format_datetime() {
        let ts = parse::parse_datetime("2024-03-15", crate::date::DATE_DIALECT).unwrap();
        let s = format_datetime(ts);
        assert!(s.starts_with("2024-03-15 00:00"), "got {}", s);
    }

    #[test]
    fn test_format_datetime_short() {
        let ts = parse::parse_datetime("2024-03-15 14:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(format_datetime_short(ts), "3-15 14:30");
        // Hour/minute are zero-padded (9:05 renders as 09:05, not 9:5).
        let ts = parse::parse_datetime("2024-03-15 09:05", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(format_datetime_short(ts), "3-15 09:05");
    }

    #[test]
    fn test_format_human_datetime_defers_to_format_datetime() {
        let ts = parse::parse_datetime("2024-03-15 14:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(format_human_datetime(ts), format_datetime(ts));
    }

    #[test]
    fn test_format_weekday() {
        // 2024-03-15 was a Friday.
        let ts = parse::parse_datetime("2024-03-15 12:00", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(format_weekday(ts), "Fr");
    }
}
