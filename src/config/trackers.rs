use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};

use super::types::{ColorBins, TrackerKind};
use crate::date::Epoch;
use cba::wbog;

/// The `colors` field as written in the config: a TOML color list
/// (`colors = ["dark_red", "dark_green"]`) or a theme-name string
/// (`colors = "rating"`, resolved against `colors.toml` in
/// `Config::init`). Defaults to the `"default"` theme when the key is
/// absent. The paired [`TrackerSetting::colors`] is `Result<ColorBins,
/// String>`: `Ok` holds an explicit palette, `Err` holds the theme name.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawColors {
    List(ColorBins),
    Name(String),
}

impl Default for RawColors {
    fn default() -> Self {
        RawColors::Name("default".to_string())
    }
}

/// `interval = { anchor = "2026-01-01T00:00:00-04:00", span = "1 day" }`.
#[derive(Debug, Clone, Copy)]
pub struct TrackerInterval {
    /// Anchor time fixing the interval phase; the slot grid runs `anchor + span*k`.
    ///
    /// Timestamps must be specified in ISO 8601 / RFC 3339 format and must explicitly include a UTC offset (e.g., ending with Z or +00:00).
    pub anchor: Epoch,
    /// The interval length (calendar-aware).
    pub span: jiff::Span,
    /// When `true`, every log within the same interval period adds a new
    /// entry and the period's value is the sum (or count, for `null`
    /// trackers) of its entries at display time. When `false` (default,
    /// replace), logging again within the same period replaces the previous
    /// entry.
    pub cumulative: bool,
}

impl PartialEq for TrackerInterval {
    fn eq(&self, other: &Self) -> bool {
        self.anchor == other.anchor
            && self.span.fieldwise() == other.span.fieldwise()
            && self.cumulative == other.cumulative
    }
}

/// The interval shape as written in the config: a table
/// `{ anchor = "...", span = "..." [, cumulative = true] }`, or the legacy
/// two-element array `["<anchor>", "<span>"]` whose elements bind
/// positionally (cumulative defaults to `false`). Tables reject unknown
/// keys via `deny_unknown_fields`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntervalRaw {
    anchor: String,
    span: String,
    #[serde(default)]
    cumulative: bool,
}

impl<'de> Deserialize<'de> for TrackerInterval {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = IntervalRaw::deserialize(deserializer)?;
        // Strict RFC 3339: an explicit UTC offset (Z or +00:00) is required.
        let ts: jiff::Timestamp = raw.anchor.parse().map_err(de::Error::custom)?;
        let span = crate::date::parse_span(&raw.span).map_err(de::Error::custom)?;
        if crate::date::span_to_db(&span) == 0 {
            return Err(de::Error::custom("interval span must be non-zero"));
        }
        Ok(TrackerInterval {
            anchor: ts.as_second(),
            span,
            cumulative: raw.cumulative,
        })
    }
}

impl Serialize for TrackerInterval {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        // Serialize back in RFC 3339 so the output re-parses with the
        // strict timestamp deserializer.
        let anchor = jiff::Timestamp::from_second(self.anchor)
            .map(|ts| ts.to_string())
            .unwrap_or_else(|_| self.anchor.to_string());
        map.serialize_entry("anchor", &anchor)?;
        map.serialize_entry("span", &crate::date::format_span(&self.span))?;
        map.serialize_entry("cumulative", &self.cumulative)?;
        map.end()
    }
}

/// `[tracker.<name>]` section — a user-defined tracker. The table key is the
/// tracker's name, used as `-<name> <value>` when logging an entry (e.g.
/// `-sleep 8` for a tracker named `sleep`).
///
/// `low`/`high` bound the entry colors (and the `strict` gate) and must use
/// the form of the tracker's kind: plain numbers for `float`/`integer`
/// (`integer` bounds must be whole), duration strings for `duration`, whole
/// numbers for `text` (message length in characters, with `strict`), plain
/// numbers for `null` in cumulative mode (count thresholds) and duration
/// strings for `null` in replace mode (seconds-from-interval-start offsets).
/// An invalid bound form is dropped with a warning; the old `min`/`max` keys
/// and the two-element interval array form are hard errors.
#[derive(Debug, Clone, Serialize)]
pub struct TrackerSetting {
    /// How often the tracker is expected to be logged, e.g.
    /// `{ anchor = "2026-01-01T00:00:00-04:00", span = "1 day" }`. With an
    /// interval, logging again within the same period replaces the previous
    /// entry (or adds a new one when `cumulative = true`); without one,
    /// every log adds a new entry. Required for `null` trackers.
    #[serde(default)]
    pub interval: Option<TrackerInterval>,
    /// What kind of value the tracker stores: `text`, `integer`, `float`,
    /// `duration`, or `null` (no value — the entry is a timestamp marker).
    pub kind: TrackerKind,
    /// Upper bound for the tracker's values, used to pick the entry's color
    /// in tracker grids and (with `strict`) to gate logging. See `low` for
    /// the accepted form per kind.
    #[serde(default)]
    pub high: Option<f64>,
    /// Lower bound for the tracker's values, used to pick the entry's color
    /// in tracker grids and (with `strict`) to gate logging. For `null`
    /// trackers in replace mode, both bounds are seconds-from-interval-start
    /// time offsets defining a circular color range (see
    /// `badge::null_tracker_color`); in cumulative mode they are plain count
    /// thresholds. For `text` trackers they are message-length thresholds in
    /// characters, meaningful only with `strict`.
    #[serde(default)]
    pub low: Option<f64>,
    /// When `true`, reject logs (CLI and TUI) whose value falls outside the
    /// inclusive span between `low` and `high` (numeric kinds), whose
    /// message length in characters does (text), or — for `null` trackers in
    /// replace mode — whose timestamp falls outside the circular
    /// `[low, high]` offset zone. Defaults to `false`. No effect without
    /// bounds; dropped at load when it cannot apply (`null` + cumulative,
    /// or `null` + replace without both bounds).
    #[serde(default)]
    pub strict: bool,
    /// Override color palette for this tracker's binning in grid/today views,
    /// or a theme name resolved against `colors.toml` at `Config::init`.
    /// `Ok` holds an explicit palette; `Err` holds the theme name (missing
    /// keys fall back to the `default` theme). After `Config::init` every
    /// value is `Ok` — read it via [`Self::colors`]. When unset the field
    /// defaults to the `default` theme. This field is `#[serde(skip)]`: it is
    /// resolved at `Config::init` and not round-tripped through the config
    /// file.
    #[serde(skip)]
    pub colors: Result<ColorBins, String>,
}

/// A `low`/`high` bound as written in the config: a TOML number or a string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawBound {
    Number(f64),
    Text(String),
}

impl std::fmt::Display for RawBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RawBound::Number(n) => write!(f, "{n}"),
            RawBound::Text(s) => write!(f, "{s:?}"),
        }
    }
}

/// Raw form of [`TrackerSetting`], used for deserialization so that the
/// `low`/`high` form rules can see the tracker's `kind` and interval mode.
/// Unknown keys (e.g. the old `min`/`max`) are hard errors.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrackerSetting {
    #[serde(default)]
    interval: Option<TrackerInterval>,
    #[serde(default)]
    kind: TrackerKind,
    #[serde(default)]
    low: Option<RawBound>,
    #[serde(default)]
    high: Option<RawBound>,
    #[serde(default)]
    strict: bool,
    #[serde(default)]
    colors: RawColors,
}

/// The `low`/`high` form a given kind/mode accepts.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BoundForm {
    /// A plain number.
    Number,
    /// A plain whole number.
    WholeNumber,
    /// A duration string (humantime), parsed to seconds.
    Duration,
}

impl BoundForm {
    fn of(kind: TrackerKind, interval: Option<TrackerInterval>) -> Self {
        match kind {
            TrackerKind::Float => BoundForm::Number,
            TrackerKind::Integer => BoundForm::WholeNumber,
            TrackerKind::Duration => BoundForm::Duration,
            TrackerKind::Text => BoundForm::WholeNumber,
            TrackerKind::Null => match interval {
                Some(iv) if iv.cumulative => BoundForm::WholeNumber,
                _ => BoundForm::Duration,
            },
        }
    }

    fn expected(&self) -> &'static str {
        match self {
            BoundForm::Number => "expected a plain number",
            BoundForm::WholeNumber => "expected a plain whole number",
            BoundForm::Duration => "expected a duration string like \"4h\"",
        }
    }
}

/// Parse one raw `low`/`high` bound against the kind's form matrix; an
/// invalid form is dropped with a warning.
fn parse_bound(which: &str, kind: TrackerKind, form: BoundForm, bound: RawBound) -> Option<f64> {
    let parsed = match (form, &bound) {
        (BoundForm::Number, RawBound::Number(n)) => Some(*n),
        (BoundForm::Number, RawBound::Text(s)) => s.parse::<f64>().ok(),
        (BoundForm::WholeNumber, RawBound::Number(n)) => whole_bound(*n),
        (BoundForm::WholeNumber, RawBound::Text(s)) => s.parse::<f64>().ok().and_then(whole_bound),
        (BoundForm::Duration, RawBound::Text(s)) => {
            humantime::parse_duration(s).ok().map(|d| d.as_secs_f64())
        }
        (BoundForm::Duration, RawBound::Number(_)) => None,
    };
    let Some(value) = parsed else {
        wbog!(
            "config";
            "Ignoring {which} bound {bound} on Tracker (kind={}): {}",
            kind.name(),
            form.expected()
        );
        return None;
    };
    Some(value)
}

fn whole_bound(n: f64) -> Option<f64> {
    (n.fract() == 0.0).then_some(n)
}

impl<'de> Deserialize<'de> for TrackerSetting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawTrackerSetting::deserialize(deserializer)?;

        // A list becomes an explicit palette; a string names a theme
        // resolved against `colors.toml` in `Config::init`.
        let colors = match raw.colors {
            RawColors::List(c) => Ok(c),
            RawColors::Name(s) => Err(s),
        };

        let form = BoundForm::of(raw.kind, raw.interval);
        let low = raw.low.and_then(|b| parse_bound("low", raw.kind, form, b));
        let high = raw
            .high
            .and_then(|b| parse_bound("high", raw.kind, form, b));

        let mut strict = raw.strict;
        // Text bounds are message-length thresholds; without strict they
        // have no meaning, so drop them.
        if raw.kind == TrackerKind::Text && !strict && (low.is_some() || high.is_some()) {
            wbog!(
                "config";
                "Ignoring low/high on text Tracker: message-length thresholds apply only with strict = true"
            );
            return Ok(TrackerSetting {
                interval: raw.interval,
                kind: raw.kind,
                low: None,
                high: None,
                strict,
                colors,
            });
        }
        // Null strict gates *when* the tracker may be logged — a circular
        // time zone that needs replace mode and both bounds.
        if raw.kind == TrackerKind::Null && strict {
            let cumulative = raw.interval.is_some_and(|iv| iv.cumulative);
            if cumulative || low.is_none() || high.is_none() {
                wbog!(
                    "config";
                    "Ignoring strict on null Tracker: strict needs replace mode with both low and high (time offsets); {}",
                    if cumulative { "it is cumulative (bounds are count thresholds)" } else { "a bound is missing" }
                );
                strict = false;
            }
        }

        Ok(TrackerSetting {
            interval: raw.interval,
            kind: raw.kind,
            low,
            high,
            strict,
            colors,
        })
    }
}

impl Default for TrackerSetting {
    fn default() -> Self {
        Self {
            interval: None,
            kind: TrackerKind::default(),
            high: None,
            low: None,
            strict: false,
            colors: Err("default".to_string()),
        }
    }
}

impl TrackerSetting {
    /// Create a tracker setting for the given value kind; all optional
    /// fields (`interval`, `low`/`high`, `strict`, `colors`) default. The
    /// `colors` field defaults to the `default` theme (resolved in
    /// `Config::init`).
    pub fn new(kind: TrackerKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    /// Set the expected logging interval (anchor + calendar span).
    pub fn with_interval(mut self, interval: TrackerInterval) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Set the upper bound for values (see [`Self::high`] for the accepted
    /// form per kind).
    pub fn with_high(mut self, high: f64) -> Self {
        self.high = Some(high);
        self
    }

    /// Set the lower bound for values (see [`Self::low`] for the accepted
    /// form per kind).
    pub fn with_low(mut self, low: f64) -> Self {
        self.low = Some(low);
        self
    }

    /// Enable the strict gate for this tracker.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Override the color palette for grid/today binning. The palette is
    /// stored directly; `Config::init` never rewrites an explicit `Ok`.
    pub fn with_colors(mut self, colors: ColorBins) -> Self {
        self.colors = Ok(colors);
        self
    }

    /// The resolved color palette for this tracker. `Config::init` resolves
    /// every `colors` field to `Ok` (theme names are looked up in
    /// `colors.toml` and missing themes fall back to the `default` theme),
    /// so this always returns the palette.
    pub fn colors(&self) -> &ColorBins {
        self.colors
            .as_ref()
            .expect("tracker colors are resolved by Config::init")
    }
}
