//! Date and datetime string parsing.
//!
//! Returns [`crate::date::Epoch`] (Unix seconds) directly — callers never
//! need to touch chrono or jiff types.

use anyhow::{Context, Result};
use jiff_english::{parse_strict, Dialect};

use crate::date::Epoch;

/// Parse a datetime string (dates, datetimes, and relative forms like
/// "yesterday" / "tomorrow 9am" / "3 days ago") to epoch seconds.
///
/// This is the single shared parsing method for every place that requests a
/// timestamp from user input (oneshot `@<time>` start time), so all of them
/// accept the same formats.
///
/// The dialect is the fixed [`crate::date::DATE_DIALECT`] constant; it only
/// matters for ambiguous slash forms like `3/5/2024` (UK: 5 March, US:
/// March 5) — ISO dates and relative phrases ("yesterday", "3 days ago")
/// parse identically under both.
///
/// Natural-language parsing is delegated to `jiff-english` (a port of
/// chrono-english on jiff); `parse_strict` resolves into a `jiff::Zoned` in
/// the local time zone and we take the epoch seconds directly. Strict means
/// the whole field must be one complete date expression — `"10pm meeting"`
/// is an error, not `10pm` (the lenient `parse_date_string` is still
/// available in jiff-english for callers that want chrono-english's
/// trailing-word tolerance). The parser also accepts the `eod` / `end` /
/// `start` time specifiers and the `hence` / `later` interval markers (see
/// `docs/datetime.md`).
pub fn parse_datetime(s: &str, dialect: Dialect) -> Result<Epoch> {
    let zdt = parse_strict(s, &jiff::Zoned::now(), dialect)
        .with_context(|| format!("Failed to parse datetime: '{}'", s))?;
    Ok(zdt.timestamp().as_second())
}

/// Parse a date string and align to the start of that day (for the
/// `im @<date>` today view).
pub fn parse_date(s: &str, dialect: Dialect) -> Result<Epoch> {
    Ok(crate::date::day_start(parse_datetime(s, dialect)?))
}

/// Parse a date string and align to the end of that day if a time is not specified.
pub fn parse_datetime_end(s: &str, dialect: Dialect) -> Result<Epoch> {
    Ok(crate::date::day_end(parse_datetime(s, dialect)?))
}

#[cfg(test)]
mod tests {
    use crate::date::format;

    use super::*;

    #[test]
    fn test_parse_datetime() {
        let ts = parse_datetime("2024-03-15", crate::date::DATE_DIALECT).unwrap();
        let formatted = format::format_datetime(ts);
        assert!(formatted.starts_with("2024-03-15"), "got {}", formatted);
    }

    #[test]
    fn test_parse_date_aligns_to_day_start() {
        // A datetime mid-day aligns to that day's start (the @<date>
        // today-view anchor).
        let ts = parse_date("2024-03-15 14:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(ts, crate::date::day_start(ts));
        assert_eq!(format::format_datetime(ts), "2024-03-15 00:00");

        // A bare date is already day-aligned; garbage still fails.
        assert!(parse_date("bogus", crate::date::DATE_DIALECT).is_err());
    }

    #[test]
    fn test_parse_datetime_english() {
        assert!(parse_datetime("2024-03-15 14:30:00", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("yesterday", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("tomorrow 9am", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("3 days ago", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("invalid date text 12345", crate::date::DATE_DIALECT).is_err());
    }

    #[test]
    fn test_parse_datetime_jiff_english_extensions() {
        // eod/end/start and hence/later are available everywhere a
        // @<time>/@<date> argument is accepted.
        assert!(parse_datetime("eod", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("tomorrow end", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("next friday start", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("3 days hence", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("2 hours later", crate::date::DATE_DIALECT).is_ok());
        // ...and the 12-hour clock is normalized: 12am is midnight, 12pm noon.
        let midnight = parse_datetime("12am", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(midnight, crate::date::day_start(midnight));
    }

    #[test]
    fn test_parse_datetime_aliases_and_negative() {
        let d = crate::date::DATE_DIALECT;
        // `y` aliases `yesterday`; both equal `-1` (one day ago).
        assert_eq!(parse_date("y", d).unwrap(), parse_date("-1", d).unwrap());
        assert_eq!(
            parse_date("yesterday", d).unwrap(),
            parse_date("-1", d).unwrap()
        );
        // `yesterweek` is a week back.
        assert_eq!(
            parse_date("yesterweek", d).unwrap(),
            parse_date("-7", d).unwrap()
        );
        // A bare negative number is days-ago, not the year 0003.
        assert_eq!(
            parse_date("-3", d).unwrap(),
            parse_date("3 days ago", d).unwrap()
        );
        // The leading '-' must be consumed: these drop it silently and
        // are rejected.
        assert!(parse_datetime("-3pm", d).is_err());
        assert!(parse_datetime("-march", d).is_err());
        assert!(parse_datetime("-yesterday", d).is_err());
        assert!(parse_datetime("-eod", d).is_err());
    }

    #[test]
    fn test_parse_datetime_is_strict() {
        // Trailing words after a bare am/pm time would parse leniently
        // (chrono-english drops them), but the main crate is strict: the
        // whole field must be one date expression.
        assert!(parse_datetime("10pm meeting", crate::date::DATE_DIALECT).is_err());
        assert!(parse_datetime("10pm", crate::date::DATE_DIALECT).is_ok());
        assert!(parse_datetime("10pm ", crate::date::DATE_DIALECT).is_ok());
    }

    #[test]
    fn test_parse_datetime_dialects_agree_on_unambiguous_forms() {
        // Same instant under both dialects for unambiguous forms.
        let uk = parse_datetime("2024-03-15 14:30:00", Dialect::Uk).unwrap();
        let us = parse_datetime("2024-03-15 14:30:00", Dialect::Us).unwrap();
        assert_eq!(uk, us);
    }
}
