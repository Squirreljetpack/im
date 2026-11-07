//! Stamped log times and civil-time windows.
//!
//! Journal entities (moods, trackers, todo completions) store the instant
//! they happened together with the UTC offset that was in effect for the
//! person who wrote them. [`LogTime`] keeps both values; the epoch orders
//! entries absolutely, while `epoch + offset` is the civil wall-clock second
//! a journal entry was written at.

use jiff::{Span, Timestamp, Zoned, civil, tz::AmbiguousOffset, tz::Offset, tz::TimeZone};

/// A log timestamp: the absolute instant plus the recorded UTC offset.
///
/// `offset` is seconds east of UTC (`-18_000` for UTC-05:00). Ordering is by
/// `epoch` only: the recorded zone is display/journal data, not a sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogTime {
    pub epoch: i64,
    pub offset: i32,
}

impl LogTime {
    /// A stored timestamp with its recorded offset.
    pub const fn new(epoch: i64, offset: i32) -> Self {
        Self { epoch, offset }
    }

    /// The current instant stamped with the system's offset.
    pub fn now() -> Self {
        let ts = Timestamp::now();
        Self::new(ts.as_second(), system_offset_secs(ts.as_second()))
    }

    /// A non-log timestamp (task schedule, interval boundary, ...) interpreted
    /// in the current system zone.
    pub fn local(epoch: i64) -> Self {
        Self::new(epoch, system_offset_secs(epoch))
    }

    /// Build from a civil wall clock and an offset.
    pub fn from_civil(civil: civil::DateTime, offset: i32) -> Result<Self, jiff::Error> {
        let zone = fixed_zone(offset)?;
        let epoch = civil.to_zoned(zone)?.timestamp().as_second();
        Ok(Self::new(epoch, offset))
    }

    /// The recorded wall clock as a civil datetime.
    pub fn civil(self) -> Result<civil::DateTime, jiff::Error> {
        Ok(self.zoned()?.datetime())
    }

    /// The instant rendered on the recorded offset's clock.
    pub fn zoned(self) -> Result<Zoned, jiff::Error> {
        Ok(Timestamp::from_second(self.epoch)?.to_zoned(fixed_zone(self.offset)?))
    }

    /// Civil wall-clock seconds: the value compared against [`CivilRange`]
    /// bounds and used for civil-date decisions.
    pub const fn civil_secs(self) -> i64 {
        self.epoch + self.offset as i64
    }

    /// The instant whose wall clock **in the current system zone** equals this
    /// log's recorded wall clock. Used only for configured tracker-interval
    /// membership and replacement slots.
    pub fn rebased(self) -> i64 {
        self.rebased_in(&TimeZone::system())
    }

    /// [`Self::rebased`] against an explicit zone (tests, callers that already
    /// resolved the system zone).
    fn rebased_in(self, zone: &TimeZone) -> i64 {
        let Ok(civil) = self.civil() else {
            return self.epoch;
        };
        let ambiguous = zone.to_ambiguous_zoned(civil);
        let resolved = match ambiguous.offset() {
            // A DST gap shifts the wall clock forward.
            AmbiguousOffset::Unambiguous { .. } | AmbiguousOffset::Gap { .. } => {
                ambiguous.compatible()
            }
            AmbiguousOffset::Fold { before, after } => {
                if self.offset == before.seconds() {
                    ambiguous.earlier()
                } else if self.offset == after.seconds() {
                    ambiguous.later()
                } else {
                    ambiguous.compatible()
                }
            }
        };
        resolved
            .map(|z| z.timestamp().as_second())
            .unwrap_or(self.epoch)
    }

    /// `true` when the recorded offset differs from the current system offset
    /// at this instant.
    pub fn is_foreign(self) -> bool {
        self.offset != system_offset_secs(self.epoch)
    }

    /// The shift from the current system wall clock to this log's recorded
    /// wall clock, in seconds: `current + shift = recorded`.
    pub fn zone_shift_secs(self) -> i32 {
        self.offset - system_offset_secs(self.epoch)
    }

    /// Compact offset label for an offset in seconds, e.g. `-3h` or `+5h30m`.
    pub fn offset_hint(self) -> String {
        offset_hint(self.offset)
    }

    /// [`Self::zone_shift_secs`] as a compact hint, e.g. `+3h` or `-5h30m`.
    pub fn zone_shift_hint(self) -> String {
        offset_hint(self.zone_shift_secs())
    }
}

/// A log row's recorded origin: the stamp it was written with and the
/// location string captured beside it. The location is opaque display
/// metadata — the writing system's IANA name, or empty when it had none — and
/// is never parsed back into a [`TimeZone`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOrigin {
    pub time: LogTime,
    pub location: String,
}

/// The system zone's IANA name, or an empty string when it has none.
///
/// This is the string committed to a log row at write time. It is opaque
/// display/journal metadata, never parsed back into [`TimeZone`].
pub fn current_location() -> String {
    TimeZone::system()
        .iana_name()
        .unwrap_or_default()
        .to_owned()
}

/// `true` when `location` names the current system zone. The empty string is
/// never current: it records that the writing system had no zone name.
pub fn location_is_current(location: &str) -> bool {
    !location.is_empty() && TimeZone::system().iana_name() == Some(location)
}

/// Inclusive civil-seconds range used by civil-horizon queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilRange {
    pub start: i64,
    pub end: i64,
}

impl CivilRange {
    /// The full civil days `start..=end`.
    pub fn from_dates(start: civil::Date, end: civil::Date) -> Result<Self, jiff::Error> {
        let start_secs = civil_midnight(start)?;
        let next = end.checked_add(Span::new().days(1))?;
        let end_secs = civil_midnight(next)? - 1;
        Ok(Self {
            start: start_secs,
            end: end_secs,
        })
    }

    pub fn contains(self, civil_secs: i64) -> bool {
        (self.start..=self.end).contains(&civil_secs)
    }

    pub fn contains_stamp(self, stamp: LogTime) -> bool {
        self.contains(stamp.civil_secs())
    }
}

fn civil_midnight(date: civil::Date) -> Result<i64, jiff::Error> {
    date.at(0, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .map(|z| z.timestamp().as_second())
}

fn fixed_zone(offset: i32) -> Result<TimeZone, jiff::Error> {
    Ok(TimeZone::fixed(Offset::from_seconds(offset)?))
}

fn offset_hint(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    if minutes == 0 {
        format!("{sign}{hours}h")
    } else {
        format!("{sign}{hours}h{minutes:02}m")
    }
}

/// The system zone's UTC offset at an instant, in seconds east of UTC.
pub fn system_offset_secs(epoch: i64) -> i32 {
    Timestamp::from_second(epoch)
        .map(|ts| TimeZone::system().to_offset(ts).seconds())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn civil_secs_is_the_recorded_wall_clock() {
        let stamp = LogTime::new(1_700_000_000, -18_000);
        assert_eq!(stamp.civil_secs(), 1_700_000_000 - 18_000);
        assert_eq!(
            stamp.civil().unwrap(),
            Timestamp::from_second(1_700_000_000)
                .unwrap()
                .to_zoned(TimeZone::fixed(Offset::from_seconds(-18_000).unwrap()))
                .datetime()
        );
    }

    #[test]
    fn offset_labels() {
        assert_eq!(LogTime::new(0, -18_000).offset_hint(), "-5h");
        assert_eq!(LogTime::new(0, 19_800).offset_hint(), "+5h30m");
    }

    #[test]
    fn the_empty_location_is_never_current() {
        let current = current_location();
        assert_eq!(location_is_current(&current), !current.is_empty());
        assert!(!location_is_current(""));
    }

    #[test]
    fn zone_shift_is_recorded_minus_current() {
        let base = system_offset_secs(1_700_000_000);
        let ahead = LogTime::new(1_700_000_000, base + 3600);
        assert_eq!(ahead.zone_shift_secs(), 3600);
        assert_eq!(ahead.zone_shift_hint(), "+1h");
        let behind = LogTime::new(1_700_000_000, base - 7200);
        assert_eq!(behind.zone_shift_hint(), "-2h");
        let local = LogTime::new(1_700_000_000, base);
        assert_eq!(local.zone_shift_secs(), 0);
    }

    #[test]
    fn rebased_prefers_the_recorded_fold_occurrence() {
        let zone = TimeZone::get("America/New_York").unwrap();
        let civil = date(2026, 11, 1).at(1, 30, 0, 0);

        let earlier = LogTime::from_civil(civil, -4 * 3600).unwrap();
        let later = LogTime::from_civil(civil, -5 * 3600).unwrap();
        assert_eq!(earlier.rebased_in(&zone), earlier.epoch);
        assert_eq!(later.rebased_in(&zone), later.epoch);
        assert_ne!(earlier.epoch, later.epoch);
    }

    #[test]
    fn rebased_uses_compatible_for_a_gap() {
        let zone = TimeZone::get("America/New_York").unwrap();
        // 02:30 does not exist on 2026-03-08 in New York; the compatible
        // resolution is the same instant as 03:30 EDT.
        let gap = date(2026, 3, 8).at(2, 30, 0, 0);
        let stamp = LogTime::from_civil(gap, -5 * 3600).unwrap();
        let expected = date(2026, 3, 8)
            .at(3, 30, 0, 0)
            .to_zoned(zone.clone())
            .unwrap()
            .timestamp()
            .as_second();
        assert_eq!(stamp.rebased_in(&zone), expected);
    }

    #[test]
    fn civil_range_covers_whole_calendar_days() {
        let start = date(2026, 3, 8);
        let range = CivilRange::from_dates(start, start).unwrap();
        assert_eq!(range.end - range.start, 86_399);
        assert!(range.contains(range.start));
        assert!(range.contains(range.end));
        assert!(!range.contains(range.start - 1));
        assert!(!range.contains(range.end + 1));
    }

    #[test]
    fn civil_range_contains_a_stamp_by_its_recorded_clock() {
        let range = CivilRange::from_dates(date(2026, 3, 8), date(2026, 3, 8)).unwrap();
        let civil = date(2026, 3, 8).at(9, 0, 0, 0);
        let inside = LogTime::from_civil(civil, 9 * 3600).unwrap();
        assert!(range.contains_stamp(inside));
        assert_eq!(inside.civil_secs(), range.start + 9 * 3600);
    }
}
