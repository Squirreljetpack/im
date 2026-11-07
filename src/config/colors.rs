use std::collections::HashMap;

use crossterm::style::Color;
use serde::{Deserialize, Serialize};

use crate::config::ColorBins;
use crate::config::DEFAULT_TRACKER_COLORS;

/// The fallback palette used when a tracker defines no colors, names a missing
/// theme, or specifies an empty list. Defined in code (not in the colors file)
/// so it is always available regardless of the on-disk colors file contents.
pub const DEFAULT_TRACKER_PALETTE: [Color; 10] = [
    Color::Rgb {
        r: 46,
        g: 48,
        b: 56,
    },
    Color::Rgb {
        r: 56,
        g: 60,
        b: 71,
    },
    Color::Rgb {
        r: 67,
        g: 74,
        b: 88,
    },
    Color::Rgb {
        r: 80,
        g: 89,
        b: 105,
    },
    Color::Rgb {
        r: 94,
        g: 103,
        b: 121,
    },
    Color::Rgb {
        r: 110,
        g: 118,
        b: 136,
    },
    Color::Rgb {
        r: 127,
        g: 134,
        b: 154,
    },
    Color::Rgb {
        r: 146,
        g: 152,
        b: 172,
    },
    Color::Rgb {
        r: 167,
        g: 172,
        b: 194,
    },
    Color::Rgb {
        r: 196,
        g: 201,
        b: 214,
    },
];

/// The colors file (`colors.toml`) — a map of theme name to the color list
/// it names. Trackers in `config.toml` reference a theme by name via their
/// `colors` field (e.g. `colors = "rating"`), or specify an inline list
/// (`colors = ["red", "blue"]`). The `default` key (when present and
/// non-empty) is the palette used when a tracker names a missing/empty theme
/// or omits `colors`; when it is absent or empty, the code-defined
/// `DEFAULT_TRACKER_PALETTE` is used instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ColorsFile(pub HashMap<String, Vec<Color>>);

impl Default for ColorsFile {
    fn default() -> Self {
        toml::from_str(DEFAULT_TRACKER_COLORS)
            .expect("bundled assets/colors.toml must parse into ColorsFile")
    }
}

impl ColorsFile {
    /// The palette named by `theme`, or `None` when the key is absent or
    /// empty.
    pub fn theme(&self, theme: &str) -> Option<&Vec<Color>> {
        self.0.get(theme).filter(|c| !c.is_empty())
    }

    /// The effective default palette: the `default` key when present and
    /// non-empty, otherwise the code-defined `DEFAULT_TRACKER_PALETTE`. This
    /// is the fallback for missing/empty themes and trackers that omit
    /// `colors`.
    pub fn default_palette(&self) -> ColorBins {
        self.theme("default")
            .map(|p| ColorBins::from(p.clone()))
            .unwrap_or_else(|| ColorBins::from(DEFAULT_TRACKER_PALETTE.to_vec()))
    }
}
