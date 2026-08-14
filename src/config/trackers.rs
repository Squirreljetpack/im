use serde::{de, ser::SerializeSeq, Deserialize, Deserializer, Serialize, Serializer};

use super::types::{ColorBins, TrackerKind};
use crate::date::Epoch;

/// `interval = ["2026-08-07T15:30:00Z", "1 day"]`.
#[derive(Debug, Clone, Copy)]
pub struct TrackerInterval {
    /// Anchor time fixing the interval phase; the slot grid runs `anchor + span*k`.
    ///
    /// Timestamps must be specified in ISO 8601 / RFC 3339 format and must explicitly include a UTC offset (e.g., ending with Z or +00:00).
    pub anchor: Epoch,
    /// The interval length (calendar-aware).
    pub span: jiff::Span,
}

impl PartialEq for TrackerInterval {
    fn eq(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.span.fieldwise() == other.span.fieldwise()
    }
}

impl<'de> Deserialize<'de> for TrackerInterval {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let parts: Vec<String> = Vec::deserialize(deserializer)?;
        if parts.len() != 2 {
            return Err(de::Error::custom(format!(
                "interval must be [\"<anchor datetime>\", \"<span>\"] (got {} elements)",
                parts.len()
            )));
        }
        // Strict RFC 3339: an explicit UTC offset (Z or +00:00) is required.
        let ts1: jiff::Timestamp = parts[0].parse().map_err(de::Error::custom)?;
        let anchor = ts1.as_second();
        let span = crate::date::parse_span(&parts[1]).map_err(de::Error::custom)?;
        if crate::date::span_to_db(&span) == 0 {
            return Err(de::Error::custom("interval span must be non-zero"));
        }
        Ok(TrackerInterval { anchor, span })
    }
}

impl Serialize for TrackerInterval {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(2))?;
        // Serialize back in RFC 3339 so the output re-parses with the
        // strict timestamp deserializer.
        let anchor = jiff::Timestamp::from_second(self.anchor)
            .map(|ts| ts.to_string())
            .unwrap_or_else(|_| self.anchor.to_string());
        seq.serialize_element(&anchor)?;
        seq.serialize_element(&crate::date::format_span(&self.span))?;
        seq.end()
    }
}

/// `[tracker.<name>]` section — a user-defined tracker. The table key is the
/// tracker's name, used as `-<name> <value>` when logging an entry (e.g.
/// `-sleep 8` for a tracker named `sleep`).
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrackerSetting {
    /// How often the tracker is expected to be logged, e.g.
    /// `["2026-03-01T00:00:00Z", "1 day"]`. With an interval, re-logging the
    /// same tracker within the same period replaces the previous entry;
    /// without one, every log adds a new entry.
    #[serde(default)]
    pub interval: Option<TrackerInterval>,
    /// What kind of value the tracker stores: `text`, `number`, `float`, or
    /// `null` (no value — the entry is a timestamp marker).
    pub kind: TrackerKind,
    /// Upper bound for the tracker's values, used to pick the entry's color
    /// in tracker grids (`number`/`float` trackers only; for `null` trackers
    /// with an interval both bounds are seconds-from-interval-start time
    /// offsets defining the circular color range — see
    /// `badge::null_tracker_color`). Accepts a plain number or a duration
    /// string (e.g. `"4h"` = 14400 s, `"30m"` = 1800 s).
    #[serde(default, deserialize_with = "deserialize_bound")]
    pub max: Option<f64>,
    /// Lower bound for the tracker's values, used to pick the entry's color
    /// in tracker grids (`number`/`float` trackers only; for `null` trackers
    /// with an interval both bounds are seconds-from-interval-start time
    /// offsets defining the circular color range). Accepts a plain number or
    /// a duration string (e.g. `"4h"` = 14400 s, `"30m"` = 1800 s).
    #[serde(default, deserialize_with = "deserialize_bound")]
    pub min: Option<f64>,
    /// Override color palette for this tracker's binning in grid/today views.
    /// When `Some`, takes precedence over `config.tasks.colors`.
    /// Must have more than 2 entries; otherwise cleared to `None` at init.
    pub colors: Option<ColorBins>,
}

/// Deserialize a tracker `min`/`max` bound: a plain number (integer or
/// float) or a humantime duration string (`"4h"`, `"30 minutes"`) parsed to
/// seconds. A missing key stays `None` (struct-level `default`).
fn deserialize_bound<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<BoundValue>::deserialize(deserializer).map(|v| v.map(|b| b.0))
}

/// Newtype wrapper for [`deserialize_bound`]: accepts numbers and
/// number-or-duration strings, sharing [`crate::date::parse_num_or_duration`]
/// with CLI `-<tracker>` value parsing.
struct BoundValue(f64);

impl<'de> Deserialize<'de> for BoundValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundVisitor;

        impl<'de> de::Visitor<'de> for BoundVisitor {
            type Value = BoundValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a number or a duration string (e.g. \"4h\", \"30 minutes\")")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(BoundValue(v as f64))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(BoundValue(v as f64))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(BoundValue(v))
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
                crate::date::parse_num_or_duration(s)
                    .map(BoundValue)
                    .map_err(|e| E::custom(format!("invalid bound '{}': {}", s, e)))
            }
        }

        deserializer.deserialize_any(BoundVisitor)
    }
}

impl TrackerSetting {
    /// Create a tracker setting for the given value kind; all optional
    /// fields (`interval`, `min`/`max`, `colors`) default to `None`.
    pub fn new(kind: TrackerKind) -> Self {
        Self {
            interval: None,
            kind,
            max: None,
            min: None,
            colors: None,
        }
    }

    /// Set the expected logging interval (anchor + calendar span).
    pub fn with_interval(mut self, interval: TrackerInterval) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Set the upper bound for values (`number`/`float` trackers only; for
    /// `null` trackers with an interval, the bound is a time offset from the
    /// interval start — see [`Self::max`]).
    pub fn with_max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Set the lower bound for values (`number`/`float` trackers only; for
    /// `null` trackers with an interval, the bound is a time offset from the
    /// interval start — see [`Self::min`]).
    pub fn with_min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }

    /// Override the color palette for grid/today binning.
    pub fn with_colors(mut self, colors: ColorBins) -> Self {
        self.colors = Some(colors);
        self
    }
}
