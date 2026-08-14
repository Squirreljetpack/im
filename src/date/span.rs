//! Jiff `Span` ↔ database (`i64`) packing, and calendar-aware interval math.
//!
//! Intervals (recurring-task and tracker intervals) are stored in the
//! database as a packed [`DbSpan`] (see [`span_to_db`] / [`db_to_span`]) —
//! the columns keep their historical `*_secs` names but no longer hold
//! seconds. All interval boundary math is calendar-based via jiff
//! ([`current_interval_start_zoned`]), so "1 day" / "1 month" intervals
//! respect DST and variable month lengths.

use jiff::{tz::TimeZone, Span, Timestamp, Unit, Zoned};

/// A `jiff::Span` packed into an `i64` for database storage.
pub type DbSpan = i64;

/// Packs Years, Months, Weeks, Days, Hours, Minutes, and Seconds into a single i64.
pub fn span_to_db(span: &Span) -> DbSpan {
    let is_neg = span.is_negative();

    // Extract absolute values for each target unit
    let years = span.get_years().unsigned_abs() as u64; // 16 bits  (0..=65535)
    let months = span.get_months().unsigned_abs() as u64; // 8 bits  (0..=255)
    let weeks = span.get_weeks().unsigned_abs() as u64; // 8 bits  (0..=255)
    let days = span.get_days().unsigned_abs() as u64; // 11 bits (0..=2047)
    let hours = span.get_hours().unsigned_abs() as u64; // 8 bits  (0..=255)
    let minutes = span.get_minutes().unsigned_abs(); // 6 bits  (0..=63)
    let seconds = span.get_seconds().unsigned_abs(); // 6 bits  (0..=63)

    let packed: u64 = ((is_neg as u64) << 63)
        | ((years & 0xFFFF) << 47)
        | ((months & 0xFF) << 39)
        | ((weeks & 0xFF) << 31)
        | ((days & 0x7FF) << 20)
        | ((hours & 0xFF) << 12)
        | ((minutes & 0x3F) << 6)
        | (seconds & 0x3F);

    packed as i64
}

/// Unpacks a DbSpan back into a jiff::Span.
pub fn db_to_span(db_span: DbSpan) -> Span {
    let raw = db_span as u64;

    let is_neg = ((raw >> 63) & 1) == 1;
    let years = ((raw >> 47) & 0xFFFF) as i16;
    let months = ((raw >> 39) & 0xFF) as i8;
    let weeks = ((raw >> 31) & 0xFF) as i8;
    let days = ((raw >> 20) & 0x7FF) as i16;
    let hours = ((raw >> 12) & 0xFF) as i16;
    let minutes = ((raw >> 6) & 0x3F) as i8;
    let seconds = (raw & 0x3F) as i8;

    let mut span = Span::new()
        .years(years)
        .months(months)
        .weeks(weeks)
        .days(days)
        .hours(hours)
        .minutes(minutes)
        .seconds(seconds);

    if is_neg {
        span = span.negate();
    }

    span
}

/// Attach the local system time zone to a unix-seconds timestamp.
pub fn zoned_from_unix_secs(unix_secs: i64) -> Result<Zoned, jiff::Error> {
    // 1. Create a UTC Timestamp from unix seconds
    let ts = Timestamp::from_second(unix_secs)?;

    // 2. Attach the local system time zone
    Ok(ts.to_zoned(TimeZone::system()))
}

/// Rough total seconds for a span, for estimation only (nominal 365.25-day
/// years and 30.44-day months; weeks/days at 24h). Unlike `Span::total`,
/// this never fails for calendar units like months.
pub fn span_rough_seconds(span: Span) -> f64 {
    f64::from(span.get_years()) * 365.25 * 86_400.0
        + f64::from(span.get_months()) * 30.44 * 86_400.0
        + f64::from(span.get_weeks()) * 7.0 * 86_400.0
        + f64::from(span.get_days()) * 86_400.0
        + f64::from(span.get_hours()) * 3600.0
        + span.get_minutes() as f64 * 60.0
        + span.get_seconds() as f64
}

/// Calculates the start time of the active interval, preserving local time and DST rules.
///
/// - `anchor`: Fixed local start time with a time zone (e.g. 2026-03-01T00:00:00[America/New_York]).
/// - `now`: Current local datetime in the same or equivalent time zone.
/// - `span`: Interval span (e.g. 1 day, 1 month, 6 hours).
pub fn current_interval_start_zoned(
    anchor: &Zoned,
    now: &Zoned,
    span: Span,
) -> Result<Zoned, jiff::Error> {
    if now < anchor {
        return Err(jiff::Error::from_args(format_args!(
            "`now` cannot be earlier than `anchor`"
        )));
    }

    // 1. Estimate how many intervals have passed using rough duration division
    // to avoid stepping one-by-one from years in the past. The estimate uses
    // nominal unit lengths (see [`span_rough_seconds`]) because `Span::total`
    // refuses calendar units without a relative reference; the fine-tuning
    // steps below correct any estimate error.
    let rough_span_secs = span_rough_seconds(span);
    let total_elapsed_secs = (now.timestamp() - anchor.timestamp())
        .total(Unit::Second)
        .unwrap_or(0.0);
    let estimated_steps = (total_elapsed_secs / rough_span_secs).floor() as i64;

    // 2. Jump close to the target interval using calendar addition
    let mut current = anchor.checked_add(span.checked_mul(estimated_steps)?)?;

    // 3. Fine-tune forward if the estimate landed slightly behind
    while let Ok(next) = current.checked_add(span) {
        if &next > now {
            break;
        }
        current = next;
    }

    // 4. Fine-tune backward if DST transition caused estimate to overshoot
    while &current > now {
        current = current.checked_sub(span)?;
    }

    Ok(current)
}

/// The index of the interval containing `t`: the whole number of `span`s
/// between `anchor` and `t`'s interval start. The boundaries
/// `anchor + span*k` are exact, so the seconds-based estimate only needs a
/// few correction steps. Unlike [`current_interval_start_zoned`], `t` may
/// precede the anchor (negative indices).
pub fn interval_index(anchor: &Zoned, t: &Zoned, span: Span) -> Result<i64, jiff::Error> {
    let rough_span_secs = span_rough_seconds(span);
    if rough_span_secs <= 0.0 {
        return Err(jiff::Error::from_args(format_args!(
            "interval span must be non-zero"
        )));
    }
    let elapsed = (t.timestamp() - anchor.timestamp())
        .total(Unit::Second)
        .unwrap_or(0.0);
    let mut k = (elapsed / rough_span_secs).floor() as i64;
    loop {
        let Ok(span_k) = span.checked_mul(k) else {
            return Err(jiff::Error::from_args(format_args!(
                "interval index out of range"
            )));
        };
        let Ok(bound_k) = anchor.checked_add(span_k) else {
            return Err(jiff::Error::from_args(format_args!(
                "interval index out of range"
            )));
        };
        let Ok(bound_k1) = bound_k.checked_add(span) else {
            return Err(jiff::Error::from_args(format_args!(
                "interval index out of range"
            )));
        };
        if bound_k <= *t && *t < bound_k1 {
            return Ok(k);
        }
        if bound_k > *t {
            k -= 1;
        } else {
            k += 1;
        }
    }
}

/// The `[start, end)` replacement slot containing `t` for an interval
/// anchored at `anchor_unix` (`None` on conversion/overflow errors).
pub fn interval_slot_unix_secs(anchor_unix: i64, span: Span, t_unix: i64) -> Option<(i64, i64)> {
    let anchor = zoned_from_unix_secs(anchor_unix).ok()?;
    let t = zoned_from_unix_secs(t_unix).ok()?;
    let k = interval_index(&anchor, &t, span).ok()?;
    let span_k = span.checked_mul(k).ok()?;
    let start = anchor.checked_add(span_k).ok()?;
    let end = start.checked_add(span).ok()?;
    Some((start.timestamp().as_second(), end.timestamp().as_second()))
}

/// The start of the interval containing `t` as unix seconds
/// (`None` when `t` precedes the anchor).
pub fn interval_start_unix_secs(anchor_unix: i64, span: Span, t_unix: i64) -> Option<i64> {
    let anchor = zoned_from_unix_secs(anchor_unix).ok()?;
    let t = zoned_from_unix_secs(t_unix).ok()?;
    current_interval_start_zoned(&anchor, &t, span)
        .ok()
        .map(|z| z.timestamp().as_second())
}

/// The end of the interval containing `t` (`interval start + span`) as unix
/// seconds (`None` when `t` precedes the anchor or the addition overflows).
pub fn interval_end_unix_secs(anchor_unix: i64, span: Span, t_unix: i64) -> Option<i64> {
    let anchor = zoned_from_unix_secs(anchor_unix).ok()?;
    let t = zoned_from_unix_secs(t_unix).ok()?;
    let start = current_interval_start_zoned(&anchor, &t, span).ok()?;
    start
        .checked_add(span)
        .ok()
        .map(|z| z.timestamp().as_second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::Date;

    fn z(date: Date) -> Zoned {
        date.at(0, 0, 0, 0).to_zoned(TimeZone::system()).unwrap()
    }

    #[test]
    fn test_span_db_roundtrip() {
        let spans = [
            Span::new().years(1),
            Span::new().months(2).days(3).hours(4),
            Span::new().weeks(1).days(2),
            Span::new().hours(6),
            Span::new().minutes(30),
            Span::new().seconds(45),
            Span::new().days(1).hours(23).minutes(59).seconds(59),
            Span::new().years(19998).months(127).weeks(127).days(2047),
        ];
        for span in spans {
            let db = span_to_db(&span);
            assert_eq!(
                db_to_span(db).fieldwise(),
                span.fieldwise(),
                "roundtrip failed for {span:?}"
            );
        }
    }

    #[test]
    fn test_span_db_negative() {
        let span = Span::new().days(1).negate();
        let db = span_to_db(&span);
        assert!(db < 0);
        assert_eq!(db_to_span(db).fieldwise(), span.fieldwise());
    }

    #[test]
    fn test_current_interval_start_daily() {
        let anchor = z(Date::new(2026, 3, 1).unwrap());
        let now = z(Date::new(2026, 3, 10).unwrap());
        let start = current_interval_start_zoned(&anchor, &now, Span::new().days(1)).unwrap();
        assert_eq!(start, z(Date::new(2026, 3, 10).unwrap()));
        // Mid-interval.
        let now = z(Date::new(2026, 3, 10).unwrap())
            .checked_add(Span::new().hours(13))
            .unwrap();
        let start = current_interval_start_zoned(&anchor, &now, Span::new().days(1)).unwrap();
        assert_eq!(start, z(Date::new(2026, 3, 10).unwrap()));
        // Before the anchor.
        let early = z(Date::new(2026, 2, 1).unwrap());
        assert!(current_interval_start_zoned(&anchor, &early, Span::new().days(1)).is_err());
    }

    #[test]
    fn test_current_interval_start_monthly() {
        let anchor = z(Date::new(2026, 1, 31).unwrap());
        // Jan 31 + 1 month = Feb 28; the interval containing Mar 10 starts Feb 28.
        let now = z(Date::new(2026, 3, 10).unwrap());
        let start = current_interval_start_zoned(&anchor, &now, Span::new().months(1)).unwrap();
        assert_eq!(start, z(Date::new(2026, 2, 28).unwrap()));
    }

    #[test]
    fn test_interval_index() {
        let anchor = z(Date::new(2026, 3, 1).unwrap());
        let t = z(Date::new(2026, 3, 10).unwrap());
        let span = Span::new().days(1);
        assert_eq!(interval_index(&anchor, &t, span).unwrap(), 9);
        assert_eq!(interval_index(&anchor, &anchor, span).unwrap(), 0);
        let t = z(Date::new(2026, 6, 1).unwrap());
        assert_eq!(interval_index(&anchor, &t, span).unwrap(), 92);
        // Before the anchor → negative index.
        let t = z(Date::new(2026, 2, 20).unwrap());
        assert_eq!(interval_index(&anchor, &t, span).unwrap(), -9);
    }

    /// Slot boundaries are the local midnight of the containing day, not
    /// uniform `anchor + k * 86_400` offsets: the Mar 1 → Mar 11 2026 window
    /// crosses the 2026-03-08 spring-forward in DST zones (e.g.
    /// America/Toronto), where the uniform expectation is off by the offset
    /// change. Guard that premise with `assert!`, then assert the
    /// calendar-correct boundary.
    #[test]
    fn test_interval_slot_unix_secs() {
        let anchor = Date::new(2026, 3, 1)
            .unwrap()
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
            .as_second();
        let t = anchor + 10 * 86_400 + 1000;
        let (start, end) = interval_slot_unix_secs(anchor, Span::new().days(1), t).unwrap();
        // DST guard: the naive uniform slot `anchor + 10 * 86_400` is only
        // valid while the offset is constant; across the 2026-03-08
        // transition it shifts by the offset change, which must be a whole
        // number of hours for any zone transitioning on that date.
        let uniform = anchor + 10 * 86_400;
        let expected_start = z(Date::new(2026, 3, 11).unwrap()).timestamp().as_second();
        assert!(
            (expected_start - uniform) % 3600 == 0,
            "system zone's offset change across Mar 1-11 2026 is not a whole hour \
             (calendar midnight {expected_start} vs uniform slot {uniform})"
        );
        assert_eq!(start, expected_start);
        // No offset change between the 10th and 11th local midnights, so the
        // slot end is exactly one day of seconds after the start.
        assert_eq!(end, expected_start + 86_400);
        // Before the anchor: slots still tile from the anchor backward.
        let (start, end) =
            interval_slot_unix_secs(anchor, Span::new().days(1), anchor - 100).unwrap();
        assert_eq!(start, anchor - 86_400);
        assert_eq!(end, anchor);
    }

    /// Same DST guard as `test_interval_slot_unix_secs`: the interval start
    /// is the local midnight of the day containing `t` (Mar 11), which in
    /// DST zones sits one offset change away from the naive uniform
    /// expectation.
    #[test]
    fn test_interval_start_unix_secs() {
        let anchor = Date::new(2026, 3, 1)
            .unwrap()
            .at(0, 0, 0, 0)
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
            .as_second();
        let t = anchor + 10 * 86_400 + 1000;
        let start = interval_start_unix_secs(anchor, Span::new().days(1), t).unwrap();
        // DST guard: the uniform slot is only valid while the offset is
        // constant; any change across the 2026-03-08 transition must be a
        // whole number of hours.
        let uniform = anchor + 10 * 86_400;
        let expected = z(Date::new(2026, 3, 11).unwrap()).timestamp().as_second();
        assert!(
            (expected - uniform) % 3600 == 0,
            "system zone's offset change across Mar 1-11 2026 is not a whole hour \
             (calendar midnight {expected} vs uniform slot {uniform})"
        );
        assert_eq!(start, expected);
        // Before anchor → None.
        assert!(interval_start_unix_secs(anchor, Span::new().days(1), anchor - 1).is_none());
    }
}
