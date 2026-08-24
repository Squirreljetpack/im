//! Parsing English dates on [jiff](https://docs.rs/jiff).
//!
//! A port of the pattern language of
//! [`chrono-english`](https://github.com/stevedonovan/chrono-english) (MIT,
//! (c) Steve Donovan) — same grammar, same dialect semantics, same error
//! strings — but resolving into [`jiff::Zoned`] instead of `chrono` types.
//! No attempt at full natural language parsing is made: only a limited set
//! of patterns is supported.
//!
//! ## Supported Formats
//!
//! `jiff-english` does _absolute_ dates: ISO-like dates "2018-04-01" and the
//! month name forms "1 April 2018" and "April 1, 2018". (There's no ambiguity
//! so both of these forms are fine). The informal "01/04/18" or American form
//! "04/01/18" is supported; there is a [`Dialect`] enum to specify which kind
//! of date English you would like to speak. Both short and long years are
//! accepted; short dates pivot between 1940 and 2040.
//!
//! Then there are _relative_ dates like 'April 1' and '9/11' (this if using
//! `Dialect::Us`). The current year is assumed, but this can be modified by
//! 'next' and 'last'. Another relative form is simply a month name like 'apr'
//! or 'April' (case-insensitive, only first three letters significant) where
//! the day is assumed to be the 1st.
//!
//! A week-day works in the same way: 'friday' means this coming Friday,
//! relative to today. 'last Friday' is unambiguous, but 'next Friday' has
//! different meanings; in the US it means the same as 'Friday' but otherwise
//! it means the Friday of next week (plus 7 days).
//!
//! Date and time can be specified also by a number of time units: "2 days",
//! "3 hours". Again, first three letters, but 'd','m' and 'y' are understood
//! (so "3h"). We make a distinction between _second_ intervals
//! (seconds,minutes,hours), _day_ intervals (days,weeks) and _month_
//! intervals (months,years). Second intervals are not followed by a time, but
//! day and month intervals can be. Without a time, a day interval has the
//! same time as the base time (which defaults to 'now'). Month intervals
//! always give us the same date, if possible — but adding a month to
//! "30 Jan" gives "28 Feb" or "29 Feb" depending if a leap year.
//!
//! Finally, dates may be followed by time. Either 'formal' like 18:03, with
//! optional seconds and fractional seconds (like 18:03:40.25) or 'informal'
//! like 6.03pm. So one gets "next friday 8pm" and so forth.
//!
//! ## Extensions over chrono-english
//!
//! - **`eod` / `end` / `start` as time-part specifiers**: `"tomorrow eod"`,
//!   `"next friday start"`, or a bare `"eod"` (today at the last moment of
//!   the day). `start` is 00:00:00.000000000, `eod`/`end` is
//!   23:59:59.999999999. Case-insensitive.
//! - **`hence` / `later` as the explicit-future counterparts of `ago`**:
//!   `"3 days ago"` negates, `"3 days hence"` / `"3 days later"` keep the
//!   sign (identical to a plain `"3 days"`). All three markers are
//!   case-insensitive.
//! - **12-hour clock fix**: chrono-english turns `12pm` into an invalid
//!   24:00 (error) and leaves `12am` as 12:00 noon. Here `12am` is midnight
//!   (00:00) and `12pm` is noon (12:00).
//!
//! ## API
//!
//! The entry point is `parse_date_string`, given the date string, a base
//! [`Zoned`](jiff::Zoned) from which relative dates and times operate, and a
//! dialect. The base time also specifies the desired timezone; the result
//! keeps it. DST gaps and folds resolve with jiff's "compatible"
//! disambiguation.
//!
//! ```
//! use jiff::civil;
//! use jiff::tz::TimeZone;
//! use jiff_english::{parse_date_string, Dialect};
//!
//! let base = civil::date(2024, 3, 14)
//!     .at(12, 34, 56, 0)
//!     .to_zoned(TimeZone::UTC)
//!     .unwrap();
//! let date_time = parse_date_string("next friday 8pm", &base, Dialect::Uk).unwrap();
//! assert_eq!(date_time.date(), civil::date(2024, 3, 22));
//! assert_eq!(date_time.time(), civil::time(20, 0, 0, 0));
//! ```
//!
//! There is a little command-line program `parse-date` in the `examples`
//! folder which can be used to play with these expressions.
//!
//! The other function, `parse_duration`, lets you access just the relative
//! part of a string like 'two days ago' or '12 hours'. If successful, returns
//! an [`Interval`], which is a number of seconds, days, or months.
//!
//! ```
//! use jiff_english::{parse_duration, Interval};
//!
//! assert_eq!(parse_duration("15m ago").unwrap(), Interval::Seconds(-15 * 60));
//! assert_eq!(parse_duration("3 days hence").unwrap(), Interval::Days(3));
//! ```

use jiff::Zoned;

mod errors;
mod parser;
mod types;

#[cfg(feature = "serde")]
pub mod serde;

use errors::*;
use types::*;

pub use errors::{DateError, DateResult, date_error, date_result};
pub use types::{Interval, Month, TimeUnit, Weekday};

#[derive(Debug, Hash, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum Dialect {
    Uk,
    Us,
}

/// Like [`parse_date_string`] but also returning the remainder of the input
/// after the date expression, in the style of
/// `chrono::NaiveDate::parse_and_remainder` (which returns the leftover
/// input verbatim, trailing whitespace included).
///
/// The parser is *lenient*: input after the last meaningful token is
/// ignored — e.g. the `"meeting"` in `"10pm meeting"` parses as `10pm`,
/// matching chrono-english, and the remainder is `" meeting"`. Use
/// [`parse_strict`] to reject non-whitespace trailing input.
pub fn parse_and_remainder<'a>(
    s: &'a str,
    now: &Zoned,
    dialect: Dialect,
) -> DateResult<(Zoned, &'a str)> {
    let mut dp = parser::DateParser::new(s);
    if let Dialect::Us = dialect {
        dp = dp.american_date();
    }
    let d = dp.parse()?;

    // we may have explicit hour:minute:sec
    let tspec = d.time.unwrap_or_else(TimeSpec::new_empty);
    let date_time = if let Some(dspec) = d.date {
        dspec
            .to_date_time(now, &tspec, dp.american)
            .or_err("bad date")?
    } else {
        // no date, time set for today's date
        tspec
            .to_date_time(now.date(), now.time_zone())
            .or_err("bad time")?
    };
    let remainder = &s[s.len() - dp.rest().len()..];
    Ok((date_time, remainder))
}

/// Lenient entry point: like [`parse_and_remainder`] but discarding the
/// remainder — trailing non-date input is ignored.
pub fn parse_date_string(s: &str, now: &Zoned, dialect: Dialect) -> DateResult<Zoned> {
    parse_and_remainder(s, now, dialect).map(|(date_time, _)| date_time)
}

/// Strict entry point: like [`parse_date_string`] but errors if any
/// non-whitespace input remains after the date expression.
pub fn parse_strict(s: &str, now: &Zoned, dialect: Dialect) -> DateResult<Zoned> {
    let (date_time, remainder) = parse_and_remainder(s, now, dialect)?;
    if remainder.trim().is_empty() {
        Ok(date_time)
    } else {
        date_result("trailing characters after date expression")
    }
}

pub fn parse_duration(s: &str) -> DateResult<Interval> {
    let mut dp = parser::DateParser::new(s);
    let d = dp.parse()?;

    if d.time.is_some() {
        return date_result("unexpected time component");
    }

    // shouldn't happen, but.
    if d.date.is_none() {
        return date_result("could not parse date");
    }

    match d.date.unwrap() {
        DateSpec::Absolute(_) => date_result("unexpected absolute date"),
        DateSpec::FromName(_) => date_result("unexpected date component"),
        DateSpec::Relative(skip) => Ok(skip.to_interval()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use jiff::civil;
    use jiff::tz::TimeZone;

    /// The fixed probe base: 2024-03-14 12:34:56 UTC — a Thursday.
    fn base() -> Zoned {
        civil::date(2024, 3, 14)
            .at(12, 34, 56, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn utc(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8, ns: i32) -> Timestamp {
        civil::date(y, mo, d)
            .at(h, mi, s, ns)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    fn expect(s: &str, want: Timestamp) {
        let got = parse_date_string(s, &base(), Dialect::Uk)
            .unwrap_or_else(|e| panic!("parse {s:?} failed: {e}"));
        assert_eq!(got.timestamp(), want, "parse {s:?}");
    }

    fn expect_with_base(s: &str, b: &Zoned, want: Timestamp) {
        let got = parse_date_string(s, b, Dialect::Uk)
            .unwrap_or_else(|e| panic!("parse {s:?} failed: {e}"));
        assert_eq!(got.timestamp(), want, "parse {s:?}");
    }

    fn err(s: &str) -> String {
        parse_date_string(s, &base(), Dialect::Uk)
            .err()
            .unwrap_or_else(|| panic!("parse {s:?} unexpectedly succeeded"))
            .to_string()
    }

    const NOON: i32 = 0;

    #[test]
    fn basics() {
        // Day of week - relative to today. May have a time part
        expect("friday", utc(2024, 3, 15, 0, 0, 0, NOON));
        expect("friday 10:30", utc(2024, 3, 15, 10, 30, 0, NOON));
        expect("friday 8pm", utc(2024, 3, 15, 20, 0, 0, NOON));

        // The day of week is the _next_ day after today, so "Tuesday" is the
        // next Tuesday after Thursday.
        expect("tues", utc(2024, 3, 19, 0, 0, 0, NOON));

        // The expression 'next Monday' is ambiguous; in the US it means the
        // day following (same as 'Monday'). But in the UK it means the day in
        // the next week.
        let got = parse_date_string("next mon", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2024, 3, 18, 0, 0, 0, NOON));
        expect("next mon", utc(2024, 3, 25, 0, 0, 0, NOON));

        expect("last fri 9.30", utc(2024, 3, 8, 9, 30, 0, NOON));

        // date expressed as month, day - relative to today. May have a time part
        let got = parse_date_string("9/11", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2024, 9, 11, 0, 0, 0, NOON));
        let got = parse_date_string("last 9/11", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2023, 9, 11, 0, 0, 0, NOON));
        let got = parse_date_string("last 9/11 9am", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2023, 9, 11, 9, 0, 0, NOON));
        expect("9/11", utc(2024, 11, 9, 0, 0, 0, NOON)); // Uk: day/month
        expect("11/9", utc(2024, 9, 11, 0, 0, 0, NOON));
        expect("last 9/11", utc(2023, 11, 9, 0, 0, 0, NOON));
        expect("3/5/2024", utc(2024, 5, 3, 0, 0, 0, NOON));
        let got = parse_date_string("3/5/2024", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2024, 3, 5, 0, 0, 0, NOON));
        expect("April 1 8.30pm", utc(2024, 4, 1, 20, 30, 0, NOON));

        // advance by time unit from today
        // without explicit time, use base time - otherwise override
        expect("2d", utc(2024, 3, 16, 12, 34, 56, NOON));
        expect("2d 03:00", utc(2024, 3, 16, 3, 0, 0, NOON));
        expect("3 weeks", utc(2024, 4, 4, 12, 34, 56, NOON));
        expect("3h", utc(2024, 3, 14, 15, 34, 56, NOON));
        expect("6 months", utc(2024, 9, 14, 0, 0, 0, NOON));
        expect("6 months ago", utc(2023, 9, 14, 0, 0, 0, NOON));
        expect("3 hours ago", utc(2024, 3, 14, 9, 34, 56, NOON));
        expect(" -3h", utc(2024, 3, 14, 9, 34, 56, NOON));
        expect(" -3 month", utc(2023, 12, 14, 0, 0, 0, NOON));

        // absolute date with year, month, day - formal ISO and informal UK or US
        expect("2017-06-30", utc(2017, 6, 30, 0, 0, 0, NOON));
        expect("30/06/17", utc(2017, 6, 30, 0, 0, 0, NOON));
        let got = parse_date_string("06/30/17", &base(), Dialect::Us).unwrap();
        assert_eq!(got.timestamp(), utc(2017, 6, 30, 0, 0, 0, NOON));

        // may be followed by time part, formal and informal
        expect("2017-06-30 08:20:30", utc(2017, 6, 30, 8, 20, 30, NOON));
        expect(
            "2017-06-30 08:20:30 +02:00",
            utc(2017, 6, 30, 6, 20, 30, NOON),
        );
        expect(
            "2017-06-30 08:20:30 +0200",
            utc(2017, 6, 30, 6, 20, 30, NOON),
        );
        expect(
            "2017-06-30 08:20:30 -0500",
            utc(2017, 6, 30, 13, 20, 30, NOON),
        );
        expect("2017-06-30T08:20:30Z", utc(2017, 6, 30, 8, 20, 30, NOON));
        expect("2017-06-30T08:20:30", utc(2017, 6, 30, 8, 20, 30, NOON));
        expect(
            "2017-06-30T08:20:30.123",
            utc(2017, 6, 30, 8, 20, 30, 123_000_000),
        );
        expect("2017-06-30 8.20", utc(2017, 6, 30, 8, 20, 0, NOON));
        expect("2017-06-30 8.30pm", utc(2017, 6, 30, 20, 30, 0, NOON));
        expect("2017-06-30 8:30pm", utc(2017, 6, 30, 20, 30, 0, NOON));
        expect("2017-06-30 2am", utc(2017, 6, 30, 2, 0, 0, NOON));
        expect("30 June 2018", utc(2018, 6, 30, 0, 0, 0, NOON));
        expect("June 30, 2018", utc(2018, 6, 30, 0, 0, 0, NOON));
        expect("June   30,    2018", utc(2018, 6, 30, 0, 0, 0, NOON));
        expect("1 April 2018", utc(2018, 4, 1, 0, 0, 0, NOON));
    }

    #[test]
    fn keywords() {
        expect("now", utc(2024, 3, 14, 12, 34, 56, NOON));
        expect("today", utc(2024, 3, 14, 12, 34, 56, NOON));
        expect("yesterday", utc(2024, 3, 13, 12, 34, 56, NOON));
        expect("tomorrow", utc(2024, 3, 15, 12, 34, 56, NOON));
        expect("tomorrow 9am", utc(2024, 3, 15, 9, 0, 0, NOON));
        expect("now 8pm", utc(2024, 3, 14, 20, 0, 0, NOON));
        expect("yesterday 3pm", utc(2024, 3, 13, 15, 0, 0, NOON));

        // `y` is an alias for `yesterday`; `yesterweek` is a week back.
        expect("y", utc(2024, 3, 13, 12, 34, 56, NOON));
        expect("yesterweek", utc(2024, 3, 7, 12, 34, 56, NOON));
    }

    #[test]
    fn negative_numbers_are_days_ago() {
        // A bare negative number is the days-ago shorthand, not a year:
        // `-3` is 3 days ago, never 0003-01-01.
        expect("-3", utc(2024, 3, 11, 12, 34, 56, NOON));
        expect("-1", utc(2024, 3, 13, 12, 34, 56, NOON));

        // The sign must be consumed: a leading '-' before anything that
        // is not a bare number or a number+unit skip is an error.
        for bad in [
            "-3pm",         // time, not a unit skip
            "-3:30",        // formal time
            "-march",       // month name
            "-fri",         // weekday name
            "-yesterday",   // shortcut
            "-eod",         // day align
            "-next friday", // direction
        ] {
            assert!(
                parse_date_string(bad, &base(), Dialect::Uk).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn bare_positive_numbers_rejected() {
        for num in ["202", "2024", "3", "0", "1000"] {
            assert!(
                parse_date_string(num, &base(), Dialect::Uk).is_err(),
                "expected bare number {num:?} to be rejected"
            );
        }
    }

    #[test]
    fn months_and_day_month() {
        expect("april", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("apr", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("APRIL", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("next April", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("last April", utc(2023, 4, 1, 0, 0, 0, NOON));
        expect("April 1", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("next April 1", utc(2024, 4, 1, 0, 0, 0, NOON));
        expect("last April 1", utc(2023, 4, 1, 0, 0, 0, NOON));
        expect("next 1 jan", utc(2025, 1, 1, 0, 0, 0, NOON));
        expect("next 31 dec", utc(2024, 12, 31, 0, 0, 0, NOON));
        expect("4 July", utc(2024, 7, 4, 0, 0, 0, NOON));
        expect("next 4 July", utc(2024, 7, 4, 0, 0, 0, NOON));
        expect("last 4 July", utc(2023, 7, 4, 0, 0, 0, NOON));
        expect("december 25", utc(2024, 12, 25, 0, 0, 0, NOON));
        expect("next 10 Dec", utc(2024, 12, 10, 0, 0, 0, NOON));
    }

    #[test]
    fn same_day_weekday_swings_on_time() {
        // Base is a Thursday; a bare weekday with no time (00:00 < 12:34)
        // rolls to next week, but a later time stays on the day.
        expect("thursday", utc(2024, 3, 21, 0, 0, 0, NOON));
        expect("thursday 13:00", utc(2024, 3, 14, 13, 0, 0, NOON));
        expect("thursday 12:00", utc(2024, 3, 21, 12, 0, 0, NOON));

        // Same, from a Friday base.
        let fri = civil::date(2024, 3, 15)
            .at(12, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base("friday", &fri, utc(2024, 3, 22, 0, 0, 0, NOON));
        expect_with_base("friday 13:00", &fri, utc(2024, 3, 15, 13, 0, 0, NOON));
        expect_with_base("friday 11:00", &fri, utc(2024, 3, 22, 11, 0, 0, NOON));
        expect_with_base("next friday", &fri, utc(2024, 3, 29, 0, 0, 0, NOON));
        expect_with_base("last friday", &fri, utc(2024, 3, 15, 0, 0, 0, NOON));
        expect_with_base("next friday 13:00", &fri, utc(2024, 3, 22, 13, 0, 0, NOON));
        expect_with_base("last friday 13:00", &fri, utc(2024, 3, 8, 13, 0, 0, NOON));
        expect_with_base("last friday 11:00", &fri, utc(2024, 3, 15, 11, 0, 0, NOON));
    }

    #[test]
    fn time_only() {
        expect("8pm", utc(2024, 3, 14, 20, 0, 0, NOON));
        expect("18:03", utc(2024, 3, 14, 18, 3, 0, NOON));
        expect("18:03:40", utc(2024, 3, 14, 18, 3, 40, NOON));
        expect("18:03:40.25", utc(2024, 3, 14, 18, 3, 40, 250_000_000));
        expect("8.30pm", utc(2024, 3, 14, 20, 30, 0, NOON));
        expect("2am", utc(2024, 3, 14, 2, 0, 0, NOON));
        expect("9.05am", utc(2024, 3, 14, 9, 5, 0, NOON));
        expect("13.30", utc(2024, 3, 14, 13, 30, 0, NOON));
        expect("12:00", utc(2024, 3, 14, 12, 0, 0, NOON));
        expect("0:00", utc(2024, 3, 14, 0, 0, 0, NOON));
        assert_eq!(err("24:00"), "bad time");
        assert_eq!(err("25:00"), "bad time");
        expect("23:59", utc(2024, 3, 14, 23, 59, 0, NOON));
    }

    #[test]
    fn twelve_hour_clock_is_normalized() {
        // chrono-english bug fixed: 12am is midnight, 12pm is noon.
        expect("12am", utc(2024, 3, 14, 0, 0, 0, NOON));
        expect("12pm", utc(2024, 3, 14, 12, 0, 0, NOON));
        expect("12:00am", utc(2024, 3, 14, 0, 0, 0, NOON));
        expect("12:00pm", utc(2024, 3, 14, 12, 0, 0, NOON));
        expect("12:30pm", utc(2024, 3, 14, 12, 30, 0, NOON));
        expect("12.00am", utc(2024, 3, 14, 0, 0, 0, NOON));
        expect("4pm", utc(2024, 3, 14, 16, 0, 0, NOON));
    }

    #[test]
    fn day_align_specifiers() {
        let eod = utc(2024, 3, 14, 23, 59, 59, 999_999_999);
        // bare specifiers apply to today
        expect("eod", eod);
        expect("end", eod);
        expect("EOD", eod);
        expect("End", eod);
        expect("start", utc(2024, 3, 14, 0, 0, 0, 0));
        expect("START", utc(2024, 3, 14, 0, 0, 0, 0));

        // after dates
        expect("tomorrow eod", utc(2024, 3, 15, 23, 59, 59, 999_999_999));
        expect("yesterday start", utc(2024, 3, 13, 0, 0, 0, 0));
        expect("next friday eod", utc(2024, 3, 22, 23, 59, 59, 999_999_999));
        expect("friday eod", utc(2024, 3, 15, 23, 59, 59, 999_999_999));
        expect("2 days eod", utc(2024, 3, 16, 23, 59, 59, 999_999_999));
        expect("6 months start", utc(2024, 9, 14, 0, 0, 0, 0));
        expect("3 weeks end", utc(2024, 4, 4, 23, 59, 59, 999_999_999));
        expect(
            "3 days hence eod",
            utc(2024, 3, 17, 23, 59, 59, 999_999_999),
        );
        expect("2024-03-15 eod", utc(2024, 3, 15, 23, 59, 59, 999_999_999));
        expect("9/11 eod", utc(2024, 11, 9, 23, 59, 59, 999_999_999));

        // same-day weekday: eod is after the base time, start is before it
        expect("thursday eod", utc(2024, 3, 14, 23, 59, 59, 999_999_999));
        expect("thursday start", utc(2024, 3, 21, 0, 0, 0, 0));
        let fri = civil::date(2024, 3, 15)
            .at(12, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base(
            "friday eod",
            &fri,
            utc(2024, 3, 15, 23, 59, 59, 999_999_999),
        );
        expect_with_base("friday start", &fri, utc(2024, 3, 22, 0, 0, 0, 0));
    }

    #[test]
    fn hence_later() {
        expect("3 days hence", utc(2024, 3, 17, 12, 34, 56, NOON));
        expect("3 days later", utc(2024, 3, 17, 12, 34, 56, NOON));
        expect("3 days ago", utc(2024, 3, 11, 12, 34, 56, NOON));
        expect("3 days AGO", utc(2024, 3, 11, 12, 34, 56, NOON));
        expect("2 hours hence", utc(2024, 3, 14, 14, 34, 56, NOON));
        expect("2 hours later", utc(2024, 3, 14, 14, 34, 56, NOON));
        expect("6 months hence", utc(2024, 9, 14, 0, 0, 0, NOON));
        expect("1 year later", utc(2025, 3, 14, 0, 0, 0, NOON));
        expect("2 days later 15:00", utc(2024, 3, 16, 15, 0, 0, NOON));
        expect("3 weeks hence", utc(2024, 4, 4, 12, 34, 56, NOON));
        expect("15m hence", utc(2024, 3, 14, 12, 49, 56, NOON));

        // the marker is the sole sign control: a leading '-' already
        // consumed the sign, so a trailing marker is a leftover token
        // (chrono-english parity: "-2 days ago" errors too).
        assert!(parse_date_string("-2 days hence", &base(), Dialect::Uk).is_err());
        assert!(parse_date_string("-2 days later", &base(), Dialect::Uk).is_err());
    }

    #[test]
    fn month_from_name() {
        // Short and full names, case-insensitive.
        assert_eq!(Month::from_name("Jan"), Some(Month::January));
        assert_eq!(Month::from_name("january"), Some(Month::January));
        assert_eq!(Month::from_name("FEB"), Some(Month::February));
        assert_eq!(Month::from_name("mar"), Some(Month::March));
        assert_eq!(Month::from_name("APR"), Some(Month::April));
        assert_eq!(Month::from_name("may"), Some(Month::May));
        assert_eq!(Month::from_name("jun"), Some(Month::June));
        assert_eq!(Month::from_name("jul"), Some(Month::July));
        assert_eq!(Month::from_name("AUG"), Some(Month::August));
        assert_eq!(Month::from_name("sep"), Some(Month::September));
        assert_eq!(Month::from_name("oct"), Some(Month::October));
        assert_eq!(Month::from_name("NOV"), Some(Month::November));
        assert_eq!(Month::from_name("december"), Some(Month::December));
        // Too short, or not a month.
        assert_eq!(Month::from_name("ja"), None);
        assert_eq!(Month::from_name("foo"), None);
        assert_eq!(Month::from_name(""), None);
        // The input must be a prefix of the full name: partial names and
        // any truncation work, unrelated words sharing the first three
        // letters do not.
        assert_eq!(Month::from_name("janu"), Some(Month::January));
        assert_eq!(Month::from_name("decemb"), Some(Month::December));
        assert_eq!(Month::from_name("junk"), None); // jun-... but not june
        assert_eq!(Month::from_name("junior"), None);
        assert_eq!(Month::from_name("sext"), None); // sep-... but not september
        // Number round-trips.
        assert_eq!(
            Month::from_number(Month::September.number()),
            Some(Month::September)
        );
        assert_eq!(Month::from_number(0), None);
        assert_eq!(Month::from_number(13), None);
    }

    #[test]
    fn weekday_from_name() {
        // Short and full names, case-insensitive.
        assert_eq!(Weekday::from_name("Mon"), Some(Weekday::Monday));
        assert_eq!(Weekday::from_name("monday"), Some(Weekday::Monday));
        assert_eq!(Weekday::from_name("TUE"), Some(Weekday::Tuesday));
        assert_eq!(Weekday::from_name("Wed"), Some(Weekday::Wednesday));
        assert_eq!(Weekday::from_name("THU"), Some(Weekday::Thursday));
        assert_eq!(Weekday::from_name("fri"), Some(Weekday::Friday));
        assert_eq!(Weekday::from_name("SAT"), Some(Weekday::Saturday));
        assert_eq!(Weekday::from_name("sunday"), Some(Weekday::Sunday));
        // Prefix rule: truncations work, unrelated words do not.
        assert_eq!(Weekday::from_name("tues"), Some(Weekday::Tuesday));
        assert_eq!(Weekday::from_name("sunny"), None); // sun-... but not sunday
        assert_eq!(Weekday::from_name("freday"), None);
        // Too short, or not a weekday.
        assert_eq!(Weekday::from_name("mo"), None);
        assert_eq!(Weekday::from_name("junk"), None);
        assert_eq!(Weekday::from_name(""), None);
    }

    #[test]
    fn time_unit_names() {
        // Single-letter shortcuts.
        assert_eq!(TimeUnit::from_name("s"), Some(TimeUnit::Second));
        assert_eq!(TimeUnit::from_name("m"), Some(TimeUnit::Minute));
        assert_eq!(TimeUnit::from_name("h"), Some(TimeUnit::Hour));
        assert_eq!(TimeUnit::from_name("d"), Some(TimeUnit::Day));
        assert_eq!(TimeUnit::from_name("w"), Some(TimeUnit::Week));
        assert_eq!(TimeUnit::from_name("y"), Some(TimeUnit::Year));
        // Prefix rule: truncations of the full name work, trailing `s`
        // is ignored (secs/mins are prefixes of the stripped stem).
        assert_eq!(TimeUnit::from_name("sec"), Some(TimeUnit::Second));
        assert_eq!(TimeUnit::from_name("secs"), Some(TimeUnit::Second));
        assert_eq!(TimeUnit::from_name("second"), Some(TimeUnit::Second));
        assert_eq!(TimeUnit::from_name("seconds"), Some(TimeUnit::Second));
        assert_eq!(TimeUnit::from_name("min"), Some(TimeUnit::Minute));
        assert_eq!(TimeUnit::from_name("mins"), Some(TimeUnit::Minute));
        assert_eq!(TimeUnit::from_name("minute"), Some(TimeUnit::Minute));
        assert_eq!(TimeUnit::from_name("hou"), Some(TimeUnit::Hour));
        assert_eq!(TimeUnit::from_name("hour"), Some(TimeUnit::Hour));
        assert_eq!(TimeUnit::from_name("hours"), Some(TimeUnit::Hour));
        assert_eq!(TimeUnit::from_name("day"), Some(TimeUnit::Day));
        assert_eq!(TimeUnit::from_name("days"), Some(TimeUnit::Day));
        assert_eq!(TimeUnit::from_name("wee"), Some(TimeUnit::Week));
        assert_eq!(TimeUnit::from_name("week"), Some(TimeUnit::Week));
        assert_eq!(TimeUnit::from_name("weeks"), Some(TimeUnit::Week));
        assert_eq!(TimeUnit::from_name("mon"), Some(TimeUnit::Month));
        assert_eq!(TimeUnit::from_name("month"), Some(TimeUnit::Month));
        assert_eq!(TimeUnit::from_name("months"), Some(TimeUnit::Month));
        assert_eq!(TimeUnit::from_name("yea"), Some(TimeUnit::Year));
        assert_eq!(TimeUnit::from_name("year"), Some(TimeUnit::Year));
        assert_eq!(TimeUnit::from_name("years"), Some(TimeUnit::Year));
        // Prefix rule rejects: two-letter forms, non-prefixes, junk.
        assert_eq!(TimeUnit::from_name("mo"), None);
        assert_eq!(TimeUnit::from_name("hr"), None);
        assert_eq!(TimeUnit::from_name("mi"), None);
        assert_eq!(TimeUnit::from_name("hourly"), None); // hou-... but not hours
        assert_eq!(TimeUnit::from_name("monthly"), None);
        assert_eq!(TimeUnit::from_name("monday"), None); // mon-... but not months
        assert_eq!(TimeUnit::from_name("sexy"), None);
        assert_eq!(TimeUnit::from_name("junk"), None);
        assert_eq!(TimeUnit::from_name(""), None);
        // Case-insensitive.
        assert_eq!(TimeUnit::from_name("MONTH"), Some(TimeUnit::Month));
        assert_eq!(TimeUnit::from_name("H"), Some(TimeUnit::Hour));
        // Interval mapping.
        assert_eq!(TimeUnit::Hour.to_interval(), Interval::Seconds(60 * 60));
        assert_eq!(TimeUnit::Week.to_interval(), Interval::Days(7));
        assert_eq!(TimeUnit::Year.to_interval(), Interval::Months(12));
    }

    #[test]
    fn month_intervals_clamp() {
        let jan31 = civil::date(2024, 1, 31)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base("1 month", &jan31, utc(2024, 2, 29, 0, 0, 0, NOON));

        let jan31_2023 = civil::date(2023, 1, 31)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base("1 month", &jan31_2023, utc(2023, 2, 28, 0, 0, 0, NOON));

        let mar31 = civil::date(2024, 3, 31)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base("1 month", &mar31, utc(2024, 4, 30, 0, 0, 0, NOON));

        let feb29 = civil::date(2024, 2, 29)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        expect_with_base("1 year", &feb29, utc(2025, 2, 28, 0, 0, 0, NOON));
    }

    #[test]
    fn dst_uses_compatible_disambiguation() {
        let ny = TimeZone::get("America/New_York").unwrap();
        // Saturday before the 2024 spring-forward (Mar 10, 2:00 -> 3:00).
        let base = civil::date(2024, 3, 9)
            .at(12, 0, 0, 0)
            .to_zoned(ny.clone())
            .unwrap();

        // "tomorrow" keeps the wall-clock time across the transition:
        // Mar 10 12:00 EDT == 16:00 UTC.
        expect_with_base("tomorrow", &base, utc(2024, 3, 10, 16, 0, 0, NOON));

        // 2:30 does not exist on Mar 10; compatible mode resolves via the
        // pre-gap offset (-05:00): 02:30 EST == 07:30 UTC.
        expect_with_base("tomorrow 2:30", &base, utc(2024, 3, 10, 7, 30, 0, NOON));

        // midnight is unambiguous: Mar 10 00:00 is still EST (the gap
        // starts at 02:00), so 00:00 EST == 05:00 UTC.
        expect_with_base("tomorrow start", &base, utc(2024, 3, 10, 5, 0, 0, 0));

        // month arithmetic is civil-aware too: Apr 9 00:00 EDT == 04:00 UTC.
        expect_with_base("1 month", &base, utc(2024, 4, 9, 4, 0, 0, 0));
    }

    #[test]
    fn errors() {
        assert_eq!(err("bananas"), "expected week day or month name");
        assert_eq!(err("2018-13-40"), "bad date");
        assert_eq!(err("2023-02-29"), "bad date");
        assert_eq!(err("Feb 30"), "bad date");
        assert_eq!(err("next today"), "expected week day or month name");
        assert_eq!(err("next week"), "expected week day or month name");
        assert_eq!(err("noon"), "expected week day or month name");
        assert_eq!(err("5th of may"), "expected month or time unit");
        assert_eq!(err("1.5 hours"), "expected am or pm");
        assert_eq!(err("June 30 2018"), "unexpected token End");
        assert!(parse_date_string("3 days ago hence", &base(), Dialect::Uk).is_err());
        assert!(parse_date_string("3 days hence ago", &base(), Dialect::Uk).is_err());
        assert!(parse_date_string("tomorrow at 9am", &base(), Dialect::Uk).is_err());
        assert!(parse_date_string("5 may 8pm", &base(), Dialect::Uk).is_err());
    }

    #[test]
    fn weekday_prefix_trap() {
        // "month" starts with the Monday prefix but is not an initial
        // substring of "monday": under the prefix rule these are errors,
        // not last/next Monday (the old first-three-letters rule parsed
        // them as Monday, chrono-english parity).
        assert_eq!(err("last month"), "expected week day or month name");
        assert_eq!(err("next month"), "expected week day or month name");
    }

    #[test]
    fn lenient_vs_strict() {
        // Lenient (chrono-english parity): trailing words after a bare
        // am/pm time are dropped.
        expect("10pm meeting", utc(2024, 3, 14, 22, 0, 0, NOON));

        // parse_and_remainder reports the leftover input, chrono-style.
        let (zdt, rest) = parse_and_remainder("10pm meeting", &base(), Dialect::Uk).unwrap();
        assert_eq!(zdt.timestamp(), utc(2024, 3, 14, 22, 0, 0, NOON));
        assert_eq!(rest, " meeting");
        let (zdt, rest) = parse_and_remainder("2 days 15:00", &base(), Dialect::Uk).unwrap();
        assert_eq!(zdt.timestamp(), utc(2024, 3, 16, 15, 0, 0, NOON));
        assert_eq!(rest, "");
        let (_, rest) = parse_and_remainder("10pm  ", &base(), Dialect::Uk).unwrap();
        assert_eq!(rest, "  "); // trailing whitespace stays in the remainder
        let (_, rest) = parse_and_remainder("next friday eod", &base(), Dialect::Uk).unwrap();
        assert_eq!(rest, "");

        // Strict: any trailing non-whitespace input is an error.
        assert_eq!(
            parse_strict("10pm meeting", &base(), Dialect::Uk)
                .unwrap_err()
                .to_string(),
            "trailing characters after date expression"
        );
        // ...but whitespace-only trailing is fine, and complete expressions
        // pass across all the grammar's shapes.
        assert!(parse_strict("10pm ", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("10pm", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("2 days 15:00", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("2024-03-15 08:20:30 +02:00", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("next friday eod", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("3 days ago", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("eod", &base(), Dialect::Uk).is_ok());
        assert!(parse_strict("April 1 8.30pm", &base(), Dialect::Uk).is_ok());
        // an offset that the lenient parser would silently drop is trailing
        // input to the strict parser
        assert!(parse_strict("8pm +02:00", &base(), Dialect::Uk).is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("6h").unwrap(), Interval::Seconds(6 * 3600));
        assert_eq!(
            parse_duration("4 hours ago").unwrap(),
            Interval::Seconds(-4 * 3600)
        );
        assert_eq!(parse_duration("5 min").unwrap(), Interval::Seconds(5 * 60));
        assert_eq!(parse_duration("10m").unwrap(), Interval::Seconds(10 * 60));
        assert_eq!(
            parse_duration("15m ago").unwrap(),
            Interval::Seconds(-15 * 60)
        );

        assert_eq!(parse_duration("1 day").unwrap(), Interval::Days(1));
        assert_eq!(parse_duration("2 days ago").unwrap(), Interval::Days(-2));
        assert_eq!(parse_duration("3 weeks").unwrap(), Interval::Days(21));
        assert_eq!(parse_duration("2 weeks ago").unwrap(), Interval::Days(-14));

        assert_eq!(parse_duration("1 month").unwrap(), Interval::Months(1));
        assert_eq!(parse_duration("6 months").unwrap(), Interval::Months(6));
        assert_eq!(parse_duration("8 years").unwrap(), Interval::Months(12 * 8));

        // hence/later keep the sign, like a bare unit
        assert_eq!(parse_duration("3 days hence").unwrap(), Interval::Days(3));
        assert_eq!(parse_duration("3 days later").unwrap(), Interval::Days(3));
        assert_eq!(
            parse_duration("2 hours later").unwrap(),
            Interval::Seconds(2 * 3600)
        );
        assert_eq!(
            parse_duration("6 months hence").unwrap(),
            Interval::Months(6)
        );
        // leading '-' consumes the sign, so a trailing marker errors
        // (see `hence_later`).
        assert!(parse_duration("-2 days hence").is_err());

        // errors
        assert_eq!(
            parse_duration("2020-01-01").err().unwrap().to_string(),
            "unexpected absolute date"
        );
        assert_eq!(
            parse_duration("2 days 15:00").err().unwrap().to_string(),
            "unexpected time component"
        );
        assert_eq!(
            parse_duration("tuesday").err().unwrap().to_string(),
            "unexpected date component"
        );
        assert_eq!(
            parse_duration("bananas").err().unwrap().to_string(),
            "expected week day or month name"
        );
        assert_eq!(
            parse_duration("eod").err().unwrap().to_string(),
            "unexpected time component"
        );
    }
}
