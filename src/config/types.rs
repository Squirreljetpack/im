use cba::define_collection_wrapper;
use crossterm::style::Color;
use serde::{Deserialize, Serialize};

/// A configurable badge: a glyph and/or color, e.g. the today view's
/// `[today_view] journal_badge`. Deserializes from any of:
///
/// - a char / single-char string (`'·'` / `"·"`) — glyph only; the color
///   defaults to `Reset` at render time;
/// - a color string (`"red"`, `"#FFB6C1"`, `"rgb_(255,182,193)"`) — color
///   only;
/// - an object with either or both fields (`{ badge = '·', color = "red" }`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BadgeSetting {
    /// The glyph rendered next to the row. `None` renders nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<char>,
    /// The glyph color. `None` renders in the default (`Reset`) color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
}

impl<'de> Deserialize<'de> for BadgeSetting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Char(char),
            Color(Color),
            Fields {
                badge: Option<char>,
                color: Option<Color>,
            },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Char(badge) => BadgeSetting {
                badge: Some(badge),
                color: None,
            },
            Repr::Color(color) => BadgeSetting {
                badge: None,
                color: Some(color),
            },
            Repr::Fields { badge, color } => BadgeSetting { badge, color },
        })
    }
}

/// One mood anchor: a mood word or phrase and the color it should produce.
/// Colors accept `#RRGGBB` hex, `rgb_(r,g,b)`, or named crossterm colors.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoodEndpoint {
    pub mood: String,
    pub color: Color,
}

/// The moods file (`[moods] source`) — one `[[pairs]]` entry per mood
/// anchor, mapping a mood word (or phrase) to the color it should produce.
///
/// The bundled `assets/moods.toml` (release) / `assets/moods.dev.toml`
/// (debug) is the default: `Default` deserializes it at runtime, replacing
/// the old build-time `default_pairs()` codegen (see `build.rs` history).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoodsFile {
    /// The anchor moods: one entry per mood.
    pub pairs: Vec<MoodEndpoint>,
}

impl Default for MoodsFile {
    fn default() -> Self {
        toml::from_str(crate::config::DEFAULT_MOODS)
            .expect("bundled assets/moods.toml must parse into MoodsFile")
    }
}

define_collection_wrapper!(
  /// A list of colors, e.g. the completion-badge bins in `[tasks] colors`
  /// (`colors = ["dark_red", "dark_yellow", "dark_green"]`).
  #[derive(Debug, Clone, Serialize, Deserialize)]
  #[serde(transparent)]
  ColorBins : Vec<Color>
);

impl Default for ColorBins {
    fn default() -> Self {
        vec![Color::DarkRed, Color::DarkYellow, Color::DarkGreen].into()
    }
}

/// Payload type for a tracker entry.
///
/// `Text` stores a string (e.g. `-accomplishment "fixed 2 bugs"`), `Integer` a
/// whole number, `Float` a decimal, `Duration` a duration string stored as
/// seconds. `low`/`high` bound the value's display color and the `strict`
/// gate: plain numbers for `Integer`/`Float`, duration strings for `Duration`
/// (seconds), whole numbers for `Text` when `strict` is set (message-length
/// thresholds in characters; bounds without `strict` are dropped at load).
/// `Null` stores no value — the entry is a timestamp marker (e.g. "sleep
/// start") and requires an interval (dropped at load otherwise); with one,
/// `low`/`high` are plain-number count thresholds in cumulative mode and
/// seconds-from-interval-start time offsets in replace mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum TrackerKind {
    #[default]
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "integer")]
    Integer,
    #[serde(rename = "float")]
    Float,
    #[serde(rename = "duration")]
    Duration,
    #[serde(rename = "null")]
    Null,
}

impl TrackerKind {
    /// The config/CLI name of this kind (lowercase).
    pub fn name(self) -> &'static str {
        match self {
            TrackerKind::Text => "text",
            TrackerKind::Integer => "integer",
            TrackerKind::Float => "float",
            TrackerKind::Duration => "duration",
            TrackerKind::Null => "null",
        }
    }
}
