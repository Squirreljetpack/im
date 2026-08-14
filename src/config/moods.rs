use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::global::Embedder;
use crate::utils::Percentage;

use super::types::{MoodEndpoint, MoodsFile};

/// `[moods]` color settings — how mood words are turned into colors from
/// the anchors in the moods file (`[moods] source`). These keys live
/// directly on the `[moods]` table (they are flattened into [`MoodConfig`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct ColorAxesSettings {
    /// A short phrase prepended to every anchor mood before it is converted
    /// to a color, so the anchors read as statements about a person. Keep it
    /// in sync with `base_string`.
    pub prefix_string: String,

    /// A neutral phrase standing for "no particular mood"; anchor colors are
    /// measured from this baseline, so moods far from it produce more vivid
    /// colors.
    pub base_string: String,

    /// How decisively the strongest anchor mood wins the final color:
    /// `1.0` mixes the contributing moods evenly, higher values let the
    /// strongest mood's color dominate.
    pub blend_steepness: f32,

    /// The maximum number of anchor moods that may contribute to a single
    /// color, strongest first.
    pub top_k: usize,

    /// How strongly emotional saliency biases the day-average embedding
    /// behind a tracker-grid dot: the per-mood weights are `s^k` where `s`
    /// is the mood's saliency, so `1` (the default) is a plain saliency
    /// weighting and higher values let the most salient moods of the day
    /// dominate its color. Degrades to a plain average when every
    /// saliency is zero.
    pub grid_blend_steepness: f32,

    /// An anchor mood must make up at least this percentage of the color
    /// mix to be included at all.
    pub min_contribution: Percentage,

    /// How much emotional intensity (saliency) moves a color away from
    /// neutral: `0` disables it entirely, `100` keeps the full effect.
    pub effective_saliency_gate: Percentage,

    /// The lightness of the neutral color used when no anchor mood matches
    /// (0–100).
    pub baseline_oklab_l: Percentage,
}

impl Default for ColorAxesSettings {
    fn default() -> Self {
        Self {
            prefix_string: "person says: ".to_string(),
            base_string: "this person feels:".to_string(),
            blend_steepness: 2.0,
            grid_blend_steepness: 1.0,
            top_k: 5,
            min_contribution: Percentage::new(7),
            effective_saliency_gate: Percentage::new(50),
            baseline_oklab_l: Percentage::new(65),
        }
    }
}

/// `[moods]` section — the color settings that derive every mood's color
/// from the anchor pairs, plus `source`, the path of the moods file
/// holding those anchors.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
#[serde(deny_unknown_fields)] // empirically flatten seems to work ok here tho docs say not
pub struct MoodConfig {
    /// The color settings — the `[moods]` keys other than `source`
    /// (flattened, so they live directly on the table).
    #[serde(flatten)]
    pub axes: ColorAxesSettings,

    /// Path of the moods file holding the `[[pairs]]` anchors, relative to
    /// the config directory. Empty (the default) uses the bundled moods
    /// file; a missing or unparsable file falls back to it as well.
    #[serde(default)]
    pub source: PathBuf,
}

impl MoodConfig {
    /// Build the color model from the configured anchors. Run automatically
    /// before any color-producing command (entry logging, today view,
    /// trackers); the returned model is threaded to the callers that use it.
    pub async fn init_with(
        &self,
        pool: &sqlx::SqlitePool,
        embedder: &Embedder,
    ) -> anyhow::Result<crate::color::ColorAxes> {
        let pairs = self.load_pairs();
        crate::color::ColorAxes::build_async(pool, embedder, &self.axes, &pairs).await
    }

    /// Resolve the anchor pairs. An empty `source` skips deserialization
    /// and uses the bundled default directly. Otherwise the `source` file
    /// (relative to the config directory) is deserialized, falling back to
    /// the bundled default when it can't be read or parsed, or when it
    /// yields no pairs (the same load-or-default pattern as the config
    /// itself, see `cba::bo::load_type_or_default`).
    pub(crate) fn load_pairs(&self) -> Vec<MoodEndpoint> {
        if self.source.as_os_str().is_empty() {
            return MoodsFile::default().pairs;
        }
        let path = crate::paths::config_dir().join(&self.source);
        let file = cba::bo::load_type_or_default(path, |s| toml::from_str::<MoodsFile>(s));
        if file.pairs.is_empty() {
            return MoodsFile::default().pairs;
        }
        file.pairs
    }
}
