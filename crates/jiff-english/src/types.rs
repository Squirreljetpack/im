//! Core types for `jiff-english`: the date/time specifications produced by
//! the parser, plus the dialect-independent lookup tables (weekdays, month
//! names, time units).
//!
//! This is a port of `chrono-english`'s `types` module, with one extension:
//! [`TimeSpec`] carries an optional [`DayAlign`] (`start` / `eod` / `end`)
//! instead of only a civil clock time.

use jiff::civil::{Date, Time};
use jiff::tz::{Offset, TimeZone};
use jiff::{Span, Zoned};

use crate::errors::*;

// implements next/last direction in expressions like 'next friday' and 'last 4 july'
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Next,
    Last,
    Here,
}

impl Direction {
    pub fn from_name(s: &str) -> Option<Direction> {
        use Direction::*;
        match s {
            "next" => Some(Next),
            "last" => Some(Last),
            _ => None,
        }
    }
}

// this is a day-month with direction, like 'next 10 Dec'
#[derive(Debug)]
pub struct YearDate {
    pub direct: Direction,
    pub month: Month,
    pub day: u32,
}

// for expressions like 'friday' and 'July' modifiable with next/last
#[derive(Debug)]
pub struct NamedDate<T> {
    pub direct: Direction,
    pub unit: T,
}

impl<T> NamedDate<T> {
    pub fn new(direct: Direction, unit: T) -> NamedDate<T> {
        NamedDate { direct, unit }
    }
}

// all expressions modifiable with next/last; 'fri', 'jul', '5 may'.
#[derive(Debug)]
pub enum ByName {
    WeekDay(NamedDate<Weekday>),
    MonthName(NamedDate<Month>),
    DayMonth(YearDate),
}

fn add_days(base: &Zoned, days: i64) -> Option<Zoned> {
    base.checked_add(Span::new().days(days)).ok()
}

fn next_last_direction<T: PartialOrd>(date: &T, base: &T, direct: Direction) -> Option<i32> {
    let mut res = None;
    if date > base {
        if direct == Direction::Last {
            res = Some(-1);
        }
    } else if date < base && direct == Direction::Next {
        res = Some(1)
    }
    res
}

impl ByName {
    pub fn from_name(s: &str, direct: Direction) -> Option<ByName> {
        Some(if let Some(wd) = Weekday::from_name(s) {
            ByName::WeekDay(NamedDate::new(direct, wd))
        } else {
            let mn = Month::from_name(s)?;
            ByName::MonthName(NamedDate::new(direct, mn))
        })
    }

    pub fn as_month(&self) -> Option<Month> {
        match *self {
            ByName::MonthName(ref nd) => Some(nd.unit),
            _ => None,
        }
    }

    pub fn from_day_month(day: u32, month: Month, direct: Direction) -> ByName {
        ByName::DayMonth(YearDate { direct, month, day })
    }

    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_date_time(self, base: &Zoned, ts: &TimeSpec, american: bool) -> DateResult<Zoned> {
        let this_year = base.date().year();
        let tz = base.time_zone();
        match self {
            ByName::WeekDay(mut nd) => {
                // a plain 'Friday' means the same as 'next Friday'.
                // an _explicit_ 'next Friday' has dialect-dependent meaning!
                // In UK English, it means 'Friday of next week',
                // but in US English, just the next Friday
                let mut extra_week = 0;
                match nd.direct {
                    Direction::Here => nd.direct = Direction::Next,
                    Direction::Next if !american => {
                        extra_week = 7;
                    }
                    _ => (),
                };
                let this_day = base.weekday().to_monday_zero_offset() as i64;
                let that_day = nd.unit.to_monday_zero_offset() as i64;
                let diff_days = that_day - this_day;
                let mut date = add_days(base, diff_days).or_err("bad date")?;
                if let Some(correct) = next_last_direction(&date, base, nd.direct) {
                    date = add_days(&date, 7 * correct as i64).or_err("bad date")?;
                }
                if extra_week > 0 {
                    date = add_days(&date, extra_week).or_err("bad date")?;
                }
                if diff_days == 0 {
                    // same day - comparing times will determine which way we swing...
                    let base_time = base.time();
                    let this_time = ts.civil_time().or_err("bad time")?;
                    if let Some(correct) = next_last_direction(&this_time, &base_time, nd.direct) {
                        date = add_days(&date, 7 * correct as i64).or_err("bad date")?;
                    }
                }
                ts.to_date_time(date.date(), tz)
            }
            ByName::MonthName(nd) => {
                let date = Date::new(this_year, nd.unit.number() as i8, 1)
                    .or_err("bad date")?
                    .at(0, 0, 0, 0)
                    .to_zoned(tz.clone())
                    .or_err("bad date")?;
                let date = if let Some(correct) = next_last_direction(&date, base, nd.direct) {
                    date.with()
                        .year(this_year + correct as i16)
                        .build()
                        .or_err("bad date")?
                } else {
                    date
                };
                ts.to_date_time(date.date(), tz)
            }
            ByName::DayMonth(yd) => {
                let date = Date::new(this_year, yd.month.number() as i8, yd.day as i8)
                    .or_err("bad date")?
                    .at(0, 0, 0, 0)
                    .to_zoned(tz.clone())
                    .or_err("bad date")?;
                let date = if let Some(correct) = next_last_direction(&date, base, yd.direct) {
                    date.with()
                        .year(this_year + correct as i16)
                        .build()
                        .or_err("bad date")?
                } else {
                    date
                };
                ts.to_date_time(date.date(), tz)
            }
        }
    }
}

#[derive(Debug)]
pub struct AbsDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl AbsDate {
    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_date(self) -> DateResult<Date> {
        let year = i16::try_from(self.year).or_err("bad date")?;
        let month = i8::try_from(self.month).or_err("bad date")?;
        let day = i8::try_from(self.day).or_err("bad date")?;
        Date::new(year, month, day).or_err("bad date")
    }
}

/// A generic amount of time, in either seconds, days, or months.
///
/// This way, a user can decide how they want to treat days (which do
/// not always have the same number of seconds) or months (which do not always
/// have the same number of days).
//
// Skipping a given number of time units.
// The subtlety is that we treat duration as seconds until we get
// to months, where we want to preserve dates. So adding a month to
// '5 May' gives '5 June'. Adding a month to '30 Jan' gives 'Feb 28' or 'Feb 29'
// depending on whether this is a leap year.
#[derive(Debug, Hash, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum Interval {
    Seconds(i32),
    Days(i32),
    Months(i32),
}

#[derive(Debug)]
pub struct Skip {
    pub unit: Interval,
    pub skip: i32,
}

impl Skip {
    pub fn to_date_time(&self, base: &Zoned, ts: &TimeSpec) -> DateResult<Zoned> {
        let tz = base.time_zone();
        match self.unit {
            Interval::Seconds(secs) => base
                .checked_add(Span::new().seconds((secs as i64) * (self.skip as i64)))
                .or_err("bad date"),
            Interval::Days(days) => {
                let date = base
                    .checked_add(Span::new().days((days as i64) * (self.skip as i64)))
                    .or_err("bad date")?;
                if ts.empty() {
                    Ok(date)
                } else {
                    ts.to_date_time(date.date(), tz)
                }
            }
            Interval::Months(mm) => {
                // jiff clamps calendar arithmetic natively: adding a month to
                // Jan 31 gives Feb 28/29, and to Mar 31 gives Apr 30.
                let date = base
                    .checked_add(Span::new().months((mm as i64) * (self.skip as i64)))
                    .or_err("bad date")?;
                ts.to_date_time(date.date(), tz)
            }
        }
    }

    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_interval(self) -> Interval {
        use Interval::*;

        match self.unit {
            Seconds(s) => Seconds(s * self.skip),
            Days(d) => Days(d * self.skip),
            Months(m) => Months(m * self.skip),
        }
    }
}

/// Day-alignment specifiers usable as a time part: `start`, `end`, `eod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayAlign {
    /// `start` — midnight (00:00:00.000000000).
    Start,
    /// `end` / `eod` — the last moment of the day (23:59:59.999999999).
    End,
}

impl DayAlign {
    /// Case-insensitive: `EOD`, `End`, `START` all work.
    pub fn from_name(s: &str) -> Option<DayAlign> {
        match s.to_ascii_lowercase().as_str() {
            "eod" | "end" => Some(DayAlign::End),
            "start" => Some(DayAlign::Start),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TimeSpec {
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    /// Fractional seconds, in microseconds (chrono-english parity).
    pub microsec: u32,
    pub empty: bool,
    /// Seconds east of UTC, when an explicit `±HH[:MM]` / `Z` offset was given.
    pub offset: Option<i64>,
    /// `start` / `eod` / `end` alignment; when set it overrides the civil
    /// clock fields above.
    pub day_align: Option<DayAlign>,
}

impl TimeSpec {
    pub fn new(hour: u32, min: u32, sec: u32, microsec: u32) -> TimeSpec {
        TimeSpec {
            hour,
            min,
            sec,
            microsec,
            empty: false,
            offset: None,
            day_align: None,
        }
    }

    pub fn new_with_offset(hour: u32, min: u32, sec: u32, offset: i64, microsec: u32) -> TimeSpec {
        TimeSpec {
            hour,
            min,
            sec,
            microsec,
            empty: false,
            offset: Some(offset),
            day_align: None,
        }
    }

    pub fn new_empty() -> TimeSpec {
        TimeSpec {
            hour: 0,
            min: 0,
            sec: 0,
            microsec: 0,
            empty: true,
            offset: None,
            day_align: None,
        }
    }

    pub fn aligned(align: DayAlign) -> TimeSpec {
        TimeSpec {
            hour: 0,
            min: 0,
            sec: 0,
            microsec: 0,
            empty: false,
            offset: None,
            day_align: Some(align),
        }
    }

    pub fn empty(&self) -> bool {
        self.empty
    }

    /// The civil wall-clock time this spec denotes, for same-day weekday
    /// comparisons: `start` is 00:00:00, `end`/`eod` is 23:59:59.999999999.
    /// Returns `None` for an out-of-range clock time (e.g. `24:00`).
    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn civil_time(&self) -> Option<Time> {
        match self.day_align {
            Some(DayAlign::Start) => Time::new(0, 0, 0, 0).ok(),
            Some(DayAlign::End) => Time::new(23, 59, 59, 999_999_999).ok(),
            None => Time::new(
                self.hour as i8,
                self.min as i8,
                self.sec as i8,
                (self.microsec * 1000) as i32,
            )
            .ok(),
        }
    }

    /// Attach this time spec to a date, resolving in `tz`.
    ///
    /// An explicit `±HH[:MM]` offset builds the instant in a fixed-offset
    /// time zone and expresses it in `tz` (chrono-english semantics).
    /// Ambiguous or gap times (DST transitions) resolve with jiff's
    /// "compatible" disambiguation.
    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_date_time(&self, date: Date, tz: &TimeZone) -> DateResult<Zoned> {
        let time = self.civil_time().or_err("bad time")?;
        let dt = time.to_datetime(date);
        if let Some(offs) = self.offset {
            let fixed =
                TimeZone::fixed(Offset::from_seconds(offs as i32).or_err("bad timezone offset")?);
            let instant = dt.to_zoned(fixed).or_err("bad time")?.timestamp();
            Ok(instant.to_zoned(tz.clone()))
        } else {
            tz.to_zoned(dt).or_err("bad time")
        }
    }
}

#[derive(Debug)]
pub enum DateSpec {
    Absolute(AbsDate), // Y M D (e.g. 2018-06-02, 4 July 2017)
    Relative(Skip),    // n U (e.g. 2min, 3 years ago, -2d)
    FromName(ByName),  // (e.g. 'next fri', 'jul')
}

impl DateSpec {
    pub fn absolute(year: u32, month: u32, day: u32) -> DateSpec {
        DateSpec::Absolute(AbsDate {
            year: year as i32,
            month,
            day,
        })
    }

    pub fn from_day_month(day: u32, month: Month, direct: Direction) -> DateSpec {
        DateSpec::FromName(ByName::from_day_month(day, month, direct))
    }

    pub fn skip(unit: Interval, skip: i32) -> DateSpec {
        DateSpec::Relative(Skip { unit, skip })
    }

    /// chrono-english parity: `to_*` methods take/consume their receiver.
    #[allow(clippy::wrong_self_convention)]
    pub fn to_date_time(self, base: &Zoned, ts: &TimeSpec, american: bool) -> DateResult<Zoned> {
        use DateSpec::*;
        match self {
            Absolute(ad) => {
                let date = ad.to_date()?;
                ts.to_date_time(date, base.time_zone())
            }
            Relative(skip) => skip.to_date_time(base, ts), // might need time
            FromName(byname) => byname.to_date_time(base, ts, american),
        }
    }
}

#[derive(Debug)]
pub struct DateTimeSpec {
    pub date: Option<DateSpec>,
    pub time: Option<TimeSpec>,
}

/// A month, used to parse month names (`"Jan"` / `"January"`,
/// case-insensitive; first three letters significant). Convert with
/// [`Month::number`] (January = 1) or [`Month::from_number`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Month {
    January,
    February,
    March,
    April,
    May,
    June,
    July,
    August,
    September,
    October,
    November,
    December,
}

impl Month {
    /// Parse a short (`"Jan"`) or full (`"January"`) month name,
    /// case-insensitive. The input must be an initial substring of a
    /// full name, at least three letters: `jan`, `janu`, `january` all
    /// work, `junk` does not. The three-letter minimum keeps the match
    /// unambiguous (`ju` is shared by june/july). Returns `None` for
    /// anything else.
    pub fn from_name(s: &str) -> Option<Month> {
        let s = s.to_ascii_lowercase();
        if s.len() < 3 {
            return None;
        }
        [
            (Month::January, "january"),
            (Month::February, "february"),
            (Month::March, "march"),
            (Month::April, "april"),
            (Month::May, "may"),
            (Month::June, "june"),
            (Month::July, "july"),
            (Month::August, "august"),
            (Month::September, "september"),
            (Month::October, "october"),
            (Month::November, "november"),
            (Month::December, "december"),
        ]
        .into_iter()
        .find(|(_, full)| full.starts_with(s.as_str()))
        .map(|(m, _)| m)
    }

    /// The month number, January = 1 … December = 12.
    pub fn number(self) -> u8 {
        match self {
            Month::January => 1,
            Month::February => 2,
            Month::March => 3,
            Month::April => 4,
            Month::May => 5,
            Month::June => 6,
            Month::July => 7,
            Month::August => 8,
            Month::September => 9,
            Month::October => 10,
            Month::November => 11,
            Month::December => 12,
        }
    }

    /// The month for a number `1..=12`; `None` outside that range.
    pub fn from_number(n: u8) -> Option<Month> {
        Some(match n {
            1 => Month::January,
            2 => Month::February,
            3 => Month::March,
            4 => Month::April,
            5 => Month::May,
            6 => Month::June,
            7 => Month::July,
            8 => Month::August,
            9 => Month::September,
            10 => Month::October,
            11 => Month::November,
            12 => Month::December,
            _ => return None,
        })
    }
}

/// A time unit in a relative expression (`2 days`, `-3h`, `1.5 hours`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl TimeUnit {
    /// Parse a unit name: the single-letter shortcut (`s`/`m`/`h`/`w`/`d`/`y`
    /// for second/minute/hour/week/day/year), or an initial substring of the
    /// full name, minimum three letters (`sec`, `secs`, `second`, `seconds`,
    /// `mon`, `month`, `months` all work; `mo`, `hr`, `hourly`, `monday` do
    /// not). A trailing `s` is ignored, so `secs`/`mins` work. The
    /// three-letter minimum rejects two-letter forms (`mo`, `hr`, `mi`)
    /// rather than guessing whether they mean the shortcut or the name.
    /// Case-insensitive. Returns `None` for anything else.
    pub fn from_name(s: &str) -> Option<TimeUnit> {
        let s = s.to_ascii_lowercase();
        if s.len() == 1 {
            return Some(match s.as_str() {
                "s" => TimeUnit::Second,
                "m" => TimeUnit::Minute,
                "h" => TimeUnit::Hour,
                "w" => TimeUnit::Week,
                "d" => TimeUnit::Day,
                "y" => TimeUnit::Year,
                _ => return None,
            });
        }
        let s = s.strip_suffix('s').unwrap_or(s.as_str());
        if s.len() < 3 {
            return None;
        }
        [
            (TimeUnit::Second, "seconds"),
            (TimeUnit::Minute, "minutes"),
            (TimeUnit::Hour, "hours"),
            (TimeUnit::Day, "days"),
            (TimeUnit::Week, "weeks"),
            (TimeUnit::Month, "months"),
            (TimeUnit::Year, "years"),
        ]
        .into_iter()
        .find(|(_, full)| full.starts_with(s))
        .map(|(u, _)| u)
    }

    /// The [`Interval`] this unit names (`hour` → 60 minutes, `week` → 7 days,
    /// `year` → 12 months).
    pub fn to_interval(self) -> Interval {
        use Interval::*;
        match self {
            TimeUnit::Second => Seconds(1),
            TimeUnit::Minute => Seconds(60),
            TimeUnit::Hour => Seconds(60 * 60),
            TimeUnit::Day => Days(1),
            TimeUnit::Week => Days(7),
            TimeUnit::Month => Months(1),
            TimeUnit::Year => Months(12),
        }
    }
}

/// A weekday, usable without the `serde` feature (jiff's own `Weekday`
/// has no serde impl; the serde-gated wrapper lives in [`crate::serde`]).
/// Parse names with [`Weekday::from_name`] and convert to jiff with
/// `jiff::civil::Weekday::from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    /// Parse a short (`"Mon"`) or full (`"Monday"`) weekday name,
    /// case-insensitive. The input must be an initial substring of a
    /// full name, at least three letters: `mon`, `mond`, `monday` all
    /// work, `moonday` does not. The three-letter minimum keeps the
    /// match unambiguous (`sa` is shared by saturday/sunday). Returns
    /// `None` for anything else.
    pub fn from_name(s: &str) -> Option<Weekday> {
        let s = s.to_ascii_lowercase();
        if s.len() < 3 {
            return None;
        }
        [
            (Weekday::Monday, "monday"),
            (Weekday::Tuesday, "tuesday"),
            (Weekday::Wednesday, "wednesday"),
            (Weekday::Thursday, "thursday"),
            (Weekday::Friday, "friday"),
            (Weekday::Saturday, "saturday"),
            (Weekday::Sunday, "sunday"),
        ]
        .into_iter()
        .find(|(_, full)| full.starts_with(s.as_str()))
        .map(|(w, _)| w)
    }

    /// Monday-zero weekday index (0 = Monday … 6 = Sunday), matching
    /// jiff's `to_monday_zero_offset`.
    pub fn to_monday_zero_offset(self) -> u32 {
        match self {
            Weekday::Monday => 0,
            Weekday::Tuesday => 1,
            Weekday::Wednesday => 2,
            Weekday::Thursday => 3,
            Weekday::Friday => 4,
            Weekday::Saturday => 5,
            Weekday::Sunday => 6,
        }
    }
}

impl From<Weekday> for jiff::civil::Weekday {
    fn from(w: Weekday) -> Self {
        match w {
            Weekday::Monday => jiff::civil::Weekday::Monday,
            Weekday::Tuesday => jiff::civil::Weekday::Tuesday,
            Weekday::Wednesday => jiff::civil::Weekday::Wednesday,
            Weekday::Thursday => jiff::civil::Weekday::Thursday,
            Weekday::Friday => jiff::civil::Weekday::Friday,
            Weekday::Saturday => jiff::civil::Weekday::Saturday,
            Weekday::Sunday => jiff::civil::Weekday::Sunday,
        }
    }
}
