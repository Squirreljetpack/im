//! Shared validation and text for the tracker value prompts (the Today
//! view's `Update` modal and the tracker picker's `Log` modal).
//!
//! Validation is the same pipeline as the CLI: `parse_tracker_value` then,
//! only when the tracker opts into `strict`, `enforce_strict`. The strict
//! error is formatted in the tracker's own units (durations as `6m 30s`,
//! text as characters) so it is actionable in the prompt.

use crate::config::{TrackerKind, TrackerSetting};
use crate::date;
use crate::db::TrackerValue;
use crate::tracker::{enforce_strict, parse_tracker_value, text_len_chars};

/// Characters accepted by a tracker kind's input modal.
pub(crate) fn chars(kind: TrackerKind) -> impl Fn(char) -> bool + Send + Sync + 'static {
    move |c: char| match kind {
        TrackerKind::Integer => c.is_ascii_digit() || c == '-',
        TrackerKind::Float => c.is_ascii_digit() || c == '-' || c == '.',
        // humantime strings like "1h 30m": digits, letters, space, dot.
        TrackerKind::Duration => c.is_ascii_alphanumeric() || c == ' ' || c == '.' || c == '-',
        TrackerKind::Text => true,
        TrackerKind::Null => false,
    }
}

/// The strict-gate measure for a parsed tracker value: the numeric score or,
/// for text, the message length in characters.
pub(crate) fn measure(value: &TrackerValue) -> f64 {
    match value {
        TrackerValue::Text(s) => text_len_chars(s) as f64,
        TrackerValue::Integer(n) => *n as f64,
        TrackerValue::Float(f) => *f,
    }
}

/// Parse `raw` for `kind` and apply the strict gate when the tracker enables
/// it. `None` means the tracker is unknown here; only the parse runs.
pub(crate) fn validate(
    name: &str,
    setting: Option<&TrackerSetting>,
    kind: TrackerKind,
    raw: &str,
) -> Result<(), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("requires a value".to_string());
    }
    let parsed = parse_tracker_value(name, kind, raw).map_err(|e| format!("{e:#}"))?;
    if let Some(setting) = setting
        && setting.strict
        && enforce_strict(name, setting, measure(&parsed)).is_err()
    {
        return Err(strict_message(name, setting, kind, &parsed));
    }
    Ok(())
}

/// A strict-gate failure phrased in the tracker's units.
pub(crate) fn strict_message(
    name: &str,
    setting: &TrackerSetting,
    kind: TrackerKind,
    value: &TrackerValue,
) -> String {
    let actual = match value {
        TrackerValue::Text(s) => format!("length {} characters", text_len_chars(s)),
        TrackerValue::Integer(n) => n.to_string(),
        TrackerValue::Float(f) if kind == TrackerKind::Duration => {
            date::format_tracker_duration(*f)
        }
        TrackerValue::Float(f) => f.to_string(),
    };
    let range = range_text(setting).unwrap_or_default();
    format!("tracker '{name}': {actual} is outside the strict range {range}")
}

fn range_text(setting: &TrackerSetting) -> Option<String> {
    let kind = setting.kind;
    match (setting.low, setting.high) {
        (Some(low), Some(high)) => {
            let (lo, hi) = (low.min(high), low.max(high));
            Some(format!(
                "{}–{}",
                format_bound(kind, lo),
                format_bound(kind, hi)
            ))
        }
        (Some(low), None) => Some(format!("≥ {}", format_bound(kind, low))),
        (None, Some(high)) => Some(format!("≤ {}", format_bound(kind, high))),
        (None, None) => None,
    }
}

fn format_bound(kind: TrackerKind, value: f64) -> String {
    match kind {
        TrackerKind::Duration => date::format_tracker_duration(value),
        TrackerKind::Text => (value.round() as i64).to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_gate_applies_only_when_enabled() {
        let mut mile = TrackerSetting::new(TrackerKind::Duration);
        mile.low = Some(240.0);
        mile.high = Some(600.0);

        // Bounds without strict are color-only: an out-of-range log is valid.
        assert!(validate("mile", Some(&mile), TrackerKind::Duration, "20m").is_ok());

        mile.strict = true;
        let err = validate("mile", Some(&mile), TrackerKind::Duration, "20m").unwrap_err();
        assert_eq!(
            err,
            "tracker 'mile': 20m is outside the strict range 4m–10m"
        );
    }
}
