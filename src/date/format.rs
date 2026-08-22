//! Formatting helpers for epoch timestamps and durations.

use crate::date::Epoch;
use jiff::civil::Weekday;

/// Format seconds as a human-readable duration string (e.g. "1 day", "2 hours").
pub fn format_duration(secs: i64) -> String {
    humantime::format_duration(std::time::Duration::from_secs(secs as u64)).to_string()
}

/// Format a remaining-time countdown for the interactive session timer.
///
/// Renders `MM:SS` when the remaining time is under one hour, or `HH:MM`
/// once it exceeds one hour (seconds are dropped, minutes zero-padded). The
/// decision is made from the passed-in `remaining_secs`, so the call is stateless.
pub fn format_countdown(remaining_secs: i64) -> String {
    let remaining = remaining_secs.max(0) as u64;
    let s = remaining % 60;
    let m = remaining / 60;
    if remaining > 3600 {
        let h = m / 60;
        let m = m % 60;
        format!("{h:02}:{m:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Format a tracker's stored duration value (seconds as f64) for display:
/// rounds fractional seconds (`6.5` → `"7s"`, `390` → `"6m 30s"`) and
/// clamps negatives to `"0s"` defensively (legacy/manual rows). Duration
/// tracker rows can never store a negative value through the normal write
/// paths; the clamp only guards hand-edited data.
pub(crate) fn format_tracker_duration(secs: f64) -> String {
    format_duration(secs.round().max(0.0) as i64)
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
/// `last:`, ...) and task dates.
///
/// Year is omitted when `ts` falls in the current calendar year.
/// When `named_months` is true, months are formatted with abbreviated names (e.g. "15 Mar 14:30").
/// When false, months/days are numeric (e.g. "03-15 14:30").
pub fn format_human_datetime(ts: Epoch, named_months: bool) -> String {
    let Ok(z) = crate::date::zoned_from_unix_secs(ts) else {
        return "--".to_string();
    };
    let current_year = crate::date::zoned_from_unix_secs(crate::date::now())
        .map(|nz| nz.year())
        .unwrap_or(z.year());

    let fmt = if z.year() == current_year {
        if named_months {
            "%d %b %H:%M"
        } else {
            "%m-%d %H:%M"
        }
    } else {
        if named_months {
            "%d %b %Y %H:%M"
        } else {
            "%Y-%m-%d %H:%M"
        }
    };

    jiff::fmt::strtime::format(fmt, &z).unwrap_or_else(|_| "--".to_string())
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
    fn test_format_tracker_duration() {
        assert_eq!(format_tracker_duration(390.0), "6m 30s");
        assert_eq!(format_tracker_duration(6.5), "7s"); // fractional seconds round
        assert_eq!(format_tracker_duration(0.0), "0s");
        assert_eq!(format_tracker_duration(-5.0), "0s"); // defensive clamp
        assert_eq!(format_tracker_duration(1.0), "1s");
    }

    #[test]
    fn test_format_countdown() {
        assert_eq!(format_countdown(60), "01:00");
        assert_eq!(format_countdown(59), "00:59");
        assert_eq!(format_countdown(0), "00:00");
        assert_eq!(format_countdown(600), "10:00");
        assert_eq!(format_countdown(3600), "60:00"); // exactly one hour stays MM:SS
        assert_eq!(format_countdown(3601), "01:00"); // over an hour drops seconds
        assert_eq!(format_countdown(7265), "02:01");
        assert_eq!(format_countdown(-5), "00:00"); // defensive clamp
    }

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
    fn test_format_human_datetime() {
        let ts = parse::parse_datetime("2024-03-15 14:30", crate::date::DATE_DIALECT).unwrap();
        let now_year = crate::date::zoned_from_unix_secs(crate::date::now())
            .unwrap()
            .year();
        if now_year == 2024 {
            assert_eq!(format_human_datetime(ts, true), "15 Mar 14:30");
            assert_eq!(format_human_datetime(ts, false), "03-15 14:30");
        } else {
            assert_eq!(format_human_datetime(ts, true), "15 Mar 2024 14:30");
            assert_eq!(format_human_datetime(ts, false), "2024-03-15 14:30");
        }
    }

    #[test]
    fn test_format_weekday() {
        // 2024-03-15 was a Friday.
        let ts = parse::parse_datetime("2024-03-15 12:00", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(format_weekday(ts), "Fr");
    }
}
