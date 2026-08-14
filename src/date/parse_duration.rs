//! Human-readable duration parsing.
//!
//! [`parse_duration_secs`] returns fixed seconds (i64) for fixed durations
//! (availability windows); [`parse_span`] returns calendar-aware
//! [`jiff::Span`]s (recurring/tracker intervals), where "1 day" is a
//! calendar day and "1 month" a calendar month.

use anyhow::{Context, Result};
use jiff::Span;

/// Parse a human-readable duration (e.g. "1 day", "2 hours") to seconds.
pub fn parse_duration_secs(s: &str) -> Result<i64> {
    let dur = humantime::parse_duration(s)
        .with_context(|| format!("Failed to parse duration: '{}'", s))?;
    Ok(dur.as_secs() as i64)
}

/// Parse a value that may be a plain number or a human-readable duration
/// (e.g. "6.5", "4m", "20h") to a float.
///
/// The string is first tried as an `f64`; if that fails it is parsed as a
/// humantime duration and converted to seconds (1.0 maps to 1 second).
/// Shared by tracker `min`/`max` config bounds and CLI `-<tracker>` value
/// parsing, so both accept the same inputs.
pub fn parse_num_or_duration(s: &str) -> Result<f64> {
    if let Ok(f) = s.parse::<f64>() {
        return Ok(f);
    }
    let dur = humantime::parse_duration(s)
        .with_context(|| format!("Failed to parse number or duration: '{}'", s))?;
    Ok(dur.as_secs_f64())
}

/// Parse a calendar-aware interval span (e.g. "1 day", "2 hours",
/// "1 month", "1 year", "1 week 2 days", "30 minutes", "45s").
///
/// Number/unit pairs are whitespace-separated ("1 day 6 hours"); a number
/// may also be glued to its unit ("1day"). Supported units: year(s), mo(nth)(s),
/// week(s), day(s), hour(s), minute(s) / m, second(s) / s. The span must be
/// positive.
pub fn parse_span(s: &str) -> Result<Span> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.is_empty() {
        anyhow::bail!("Failed to parse interval: '{}'", s);
    }

    let mut span = Span::new();
    let mut i = 0;
    let mut any = false;
    while i < tokens.len() {
        let token = tokens[i];
        // Split a token like "1day" into ("1", "day"); otherwise expect a
        // number followed by a separate unit word.
        let (num_str, unit_str) = if let Some(split) = token.find(|c: char| c.is_ascii_alphabetic())
        {
            (&token[..split], &token[split..])
        } else if i + 1 < tokens.len() {
            (token, tokens[i + 1])
        } else {
            anyhow::bail!("Failed to parse interval: '{}' (dangling number)", s);
        };

        let n: i64 = num_str
            .trim()
            .parse()
            .with_context(|| format!("Failed to parse interval: '{}'", s))?;
        if n <= 0 {
            anyhow::bail!("Interval must be positive (got '{}')", s);
        }

        let unit = unit_str.to_ascii_lowercase();
        span = match unit.as_str() {
            "y" | "year" | "years" => span.years(n),
            "mo" | "month" | "months" => span.months(n),
            "w" | "week" | "weeks" => span.weeks(n),
            "d" | "day" | "days" => span.days(n),
            "h" | "hour" | "hours" => span.hours(n),
            "m" | "minute" | "minutes" | "min" => span.minutes(n),
            "s" | "sec" | "second" | "seconds" => span.seconds(n),
            _ => anyhow::bail!(
                "Failed to parse interval: '{}' (unknown unit '{}')",
                s,
                unit_str
            ),
        };
        any = true;
        i += if token.bytes().any(|b| b.is_ascii_alphabetic()) {
            1
        } else {
            2
        };
    }

    if !any {
        anyhow::bail!("Failed to parse interval: '{}'", s);
    }
    Ok(span)
}

/// Format a calendar span as a human-readable interval (e.g. "1 day",
/// "2 hours", "1 month"). Only nonzero units are shown.
pub fn format_span(span: &Span) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |n: i64, unit: &str| {
        if n != 0 {
            parts.push(format!(
                "{} {}{}",
                n,
                unit,
                if n.abs() == 1 { "" } else { "s" }
            ));
        }
    };
    push(i64::from(span.get_years()), "year");
    push(i64::from(span.get_months()), "month");
    push(i64::from(span.get_weeks()), "week");
    push(i64::from(span.get_days()), "day");
    push(i64::from(span.get_hours()), "hour");
    push(span.get_minutes(), "minute");
    push(span.get_seconds(), "second");
    if parts.is_empty() {
        "0 seconds".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_secs() {
        assert_eq!(parse_duration_secs("1 day").unwrap(), 86400);
        assert_eq!(parse_duration_secs("2 hours").unwrap(), 7200);
        assert_eq!(parse_duration_secs("30 minutes").unwrap(), 1800);
        assert_eq!(parse_duration_secs("1d").unwrap(), 86400);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
    }

    #[test]
    fn test_parse_num_or_duration() {
        // Plain numbers win as-is.
        assert_eq!(parse_num_or_duration("0").unwrap(), 0.0);
        assert_eq!(parse_num_or_duration("10").unwrap(), 10.0);
        assert_eq!(parse_num_or_duration("6.5").unwrap(), 6.5);
        assert_eq!(parse_num_or_duration("-3").unwrap(), -3.0);
        // Duration strings map to seconds (1.0 = 1 second).
        assert_eq!(parse_num_or_duration("4m").unwrap(), 240.0);
        assert_eq!(parse_num_or_duration("20h").unwrap(), 72000.0);
        assert_eq!(parse_num_or_duration("30 minutes").unwrap(), 1800.0);
        assert_eq!(parse_num_or_duration("1.5s").unwrap(), 1.5);
        // A string that is neither is an error.
        assert!(parse_num_or_duration("").is_err());
        assert!(parse_num_or_duration("bogus").is_err());
    }

    #[test]
    fn test_parse_span_units() {
        assert_eq!(
            parse_span("2 hours").unwrap().fieldwise(),
            Span::new().hours(2).fieldwise()
        );
        assert_eq!(
            parse_span("30 minutes").unwrap().fieldwise(),
            Span::new().minutes(30).fieldwise()
        );
        assert_eq!(
            parse_span("1 month").unwrap().fieldwise(),
            Span::new().months(1).fieldwise()
        );
        assert_eq!(
            parse_span("1 year").unwrap().fieldwise(),
            Span::new().years(1).fieldwise()
        );
        assert_eq!(
            parse_span("1 week").unwrap().fieldwise(),
            Span::new().weeks(1).fieldwise()
        );
        assert_eq!(
            parse_span("1 week 2 days").unwrap().fieldwise(),
            Span::new().weeks(1).days(2).fieldwise()
        );
        // Glued forms.
        assert_eq!(
            parse_span("1day").unwrap().fieldwise(),
            Span::new().days(1).fieldwise()
        );
        assert_eq!(
            parse_span("2h").unwrap().fieldwise(),
            Span::new().hours(2).fieldwise()
        );
        assert_eq!(
            parse_span("45s").unwrap().fieldwise(),
            Span::new().seconds(45).fieldwise()
        );
        // Case-insensitive units.
        assert_eq!(
            parse_span("1 Day").unwrap().fieldwise(),
            Span::new().days(1).fieldwise()
        );
        // Errors.
        assert!(parse_span("").is_err());
        assert!(parse_span("1").is_err());
        assert!(parse_span("day").is_err());
        assert!(parse_span("0 days").is_err());
        assert!(parse_span("-1 day").is_err());
        assert!(parse_span("1 fortnight").is_err());
        assert!(parse_span("1 2").is_err());
    }

    #[test]
    fn test_format_span() {
        assert_eq!(format_span(&Span::new().days(1)), "1 day");
        assert_eq!(format_span(&Span::new().hours(2)), "2 hours");
        assert_eq!(format_span(&Span::new().months(1)), "1 month");
        assert_eq!(format_span(&Span::new().weeks(1).days(2)), "1 week 2 days");
        assert_eq!(format_span(&Span::new()), "0 seconds");
    }
}
