//! Unified date/time utilities — module root.
//!
//! All wrappers return [`Epoch`] for timestamps and seconds (i64) for durations.
//! No chrono, humantime, or jiff types leak to callers — all formatting and
//! parsing is encapsulated here. Calendar math (day/week/month boundaries,
//! interval arithmetic) runs on jiff with the local system time zone, so
//! DST transitions and variable month lengths are handled correctly.
//!
//! Sub-modules:
//! - [`parse`] — date & datetime string parsing
//! - [`parse_duration`] — human-readable duration parsing
//! - [`format`] — epoch/duration formatting
//! - [`span`] — jiff `Span` ↔ database packing and interval math

pub mod format;
pub mod parse;
pub mod parse_duration;
pub mod span;

/// Type alias for Unix epoch seconds.
pub type Epoch = i64;

/// A `jiff::Span` packed into an `i64` for database storage
/// (see [`span::span_to_db`]).
pub type DbSpan = i64;

/// Date formatting dialect for English locales.
///
/// `Us` uses month-first ordering (`Aug 6`); `Uk` uses day-first
/// ordering (`6 Aug`).
pub const DATE_DIALECT: jiff_english::Dialect = jiff_english::Dialect::Uk;

// Re-export sub-module functions at the crate::date level.
pub use format::*;
pub use parse::*;
pub use parse_duration::*;
pub use span::*;

use jiff::{Span, Unit, Zoned, civil::Weekday};

/// The current local zoned datetime.
fn local_now() -> Zoned {
    Zoned::now()
}

/// Current Unix epoch timestamp (seconds).
pub fn now() -> Epoch {
    jiff::Timestamp::now().as_second()
}

/// Epoch seconds for start of today (midnight local time).
pub fn today_start() -> Epoch {
    local_now()
        .start_of_day()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for end of today (23:59:59 local time).
pub fn today_end() -> Epoch {
    local_now()
        .end_of_day()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for the Monday of the current week (midnight).
pub fn week_monday() -> Epoch {
    week_start(Weekday::Monday)
}

/// Epoch seconds for the start of the current week (midnight), where
/// the week begins on `weekday` (config.grid.week_start).
pub fn week_start(weekday: Weekday) -> Epoch {
    let now_z = local_now();
    let today_offset = now_z.weekday().to_monday_zero_offset() as i64;
    let target_offset = weekday.to_monday_zero_offset() as i64;
    let back = (today_offset - target_offset).rem_euclid(7);
    now_z
        .checked_sub(Span::new().days(back))
        .and_then(|z| z.start_of_day())
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for the start of the rolling month window (the subrepo's
/// "last 4 weeks" view): `today - 27` days advanced to `weekday`.
pub fn rolling_month_start(weekday: Weekday) -> Epoch {
    let now_z = local_now();
    let mut start = now_z.checked_sub(Span::new().days(27)).unwrap_or(now_z);
    let mut guard = 0;
    while start.weekday() != weekday && guard < 8 {
        start = start.checked_add(Span::new().days(1)).unwrap_or(start);
        guard += 1;
    }
    start
        .start_of_day()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for the start of the rolling year window (the subrepo's
/// year view): `today - 364` days walked back to `weekday`, so the window
/// opens on a full week (no leading blanks).
pub fn rolling_year_start(weekday: Weekday) -> Epoch {
    let now_z = local_now();
    let mut start = now_z.checked_sub(Span::new().days(364)).unwrap_or(now_z);
    let mut guard = 0;
    while start.weekday() != weekday && guard < 8 {
        start = start.checked_sub(Span::new().days(1)).unwrap_or(start);
        guard += 1;
    }
    start
        .start_of_day()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for the `weekday` on or before January 1 of the current year.
/// Used for year grids aligned to a full week start (so the grid never opens
/// with blank cells in the first column).
pub fn aligned_year_start(weekday: Weekday) -> Epoch {
    let now_z = local_now();
    let jan1 = now_z
        .with()
        .month(1)
        .day(1)
        .hour(0)
        .minute(0)
        .second(0)
        .nanosecond(0)
        .build()
        .unwrap_or(now_z);
    let mut start = jan1;
    let mut guard = 0;
    while start.weekday() != weekday && guard < 8 {
        start = start.checked_sub(Span::new().days(1)).unwrap_or(start);
        guard += 1;
    }
    start.timestamp().as_second()
}

/// Epoch seconds for the first day of the current month (midnight).
pub fn month_start() -> Epoch {
    local_now()
        .with()
        .day(1)
        .hour(0)
        .minute(0)
        .second(0)
        .nanosecond(0)
        .build()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for the last day of the current month (23:59:59).
pub fn month_end() -> Epoch {
    next_month_start_secs().map(|s| s - 1).unwrap_or_else(now)
}

/// Epoch seconds for the start of the next month (midnight), used to derive
/// month boundaries without civil-date gymnastics.
fn next_month_start_secs() -> Option<Epoch> {
    let now_z = local_now();
    let first = now_z
        .with()
        .day(1)
        .hour(0)
        .minute(0)
        .second(0)
        .nanosecond(0)
        .build()
        .ok()?;
    first
        .checked_add(Span::new().months(1))
        .ok()
        .map(|z| z.timestamp().as_second())
}

/// Epoch seconds for January 1 of the current year (midnight).
pub fn year_start() -> Epoch {
    local_now()
        .with()
        .month(1)
        .day(1)
        .hour(0)
        .minute(0)
        .second(0)
        .nanosecond(0)
        .build()
        .map(|z| z.timestamp().as_second())
        .unwrap_or_else(|_| now())
}

/// Epoch seconds for December 31 of the current year (23:59:59).
pub fn year_end() -> Epoch {
    let now_z = local_now();
    let jan1 = now_z
        .with()
        .month(1)
        .day(1)
        .hour(0)
        .minute(0)
        .second(0)
        .nanosecond(0)
        .build()
        .unwrap_or(now_z);
    jan1.checked_add(Span::new().years(1))
        .map(|z| z.timestamp().as_second() - 1)
        .unwrap_or_else(|_| now())
}

/// Get the epoch seconds for start of the day containing the given timestamp.
pub fn day_start(ts: Epoch) -> Epoch {
    zoned_from_unix_secs(ts)
        .and_then(|z| z.start_of_day())
        .map(|z| z.timestamp().as_second())
        .unwrap_or(ts)
}

/// Get the epoch seconds for end of the day (23:59:59) containing the given timestamp.
pub fn day_end(ts: Epoch) -> Epoch {
    zoned_from_unix_secs(ts)
        .and_then(|z| z.end_of_day())
        .map(|z| z.timestamp().as_second())
        .unwrap_or(ts)
}

/// Total seconds in `span` for rough-duration comparisons (24-hour days;
/// calendar units like months use their nominal length). Returns `None` for
/// a zero span.
pub fn span_total_seconds(span: Span) -> Option<f64> {
    span.total((Unit::Second, jiff::SpanRelativeTo::days_are_24_hours()))
        .ok()
        .filter(|s| *s > 0.0)
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_nonzero() {
        assert!(now() > 1_700_000_000);
    }

    #[test]
    fn test_today_bounds() {
        let start = today_start();
        let end = today_end();
        assert!(start <= end);
        assert_eq!(end - start, 86399);
    }

    #[test]
    fn test_week_start_monday_alignment() {
        // The returned day must be a Monday.
        let ws = week_start(Weekday::Monday);
        let z = zoned_from_unix_secs(ws).unwrap();
        assert_eq!(z.weekday(), Weekday::Monday);
        assert_eq!(z.time(), jiff::civil::Time::midnight());
    }

    #[test]
    fn test_month_bounds() {
        let start = month_start();
        let end = month_end();
        assert!(start <= end);
        assert!(end - start >= 27 * 86_400 - 1, "month too short");
        assert!(end - start <= 31 * 86_400, "month too long");
    }

    #[test]
    fn test_year_bounds() {
        let start = year_start();
        let end = year_end();
        assert!(start <= end);
        assert!(end - start >= 364 * 86_400, "year too short");
        assert!(end - start <= 366 * 86_400, "year too long");
    }

    #[test]
    fn test_day_bounds() {
        let ts = today_start() + 12 * 3600;
        assert_eq!(day_start(ts), today_start());
        assert_eq!(day_end(ts), today_end());
        assert_eq!(day_end(ts) - day_start(ts), 86399);
    }
}
