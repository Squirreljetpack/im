#![allow(clippy::needless_range_loop)]
//! Mood color computation via NNLS basis-ray regression & saliency scaling.
//!
//! Mood strings are embedded with the nomic-embed-text-v1.5 model, projected onto a
//! user-defined set of basis `MoodEndpoint`s via Non-Negative Least Squares (NNLS),
//! filtered by contribution %, blended using power-weighted centroid mixing in Oklab,
//! and rescaled by predicted emotional saliency, gated by the configured
//! `emotional_saliency_gate` P (Seff = 1 + P*(S - 1)):
//!
//! L_final = L_neutral + Seff * (L_blended - L_neutral)
//! a_final = Seff * a_blended
//! b_final = Seff * b_blended

mod blend;
pub(crate) mod conversion;
mod nnls;

pub use blend::{average_oklab, blend_weights, lerp_oklab};
pub use nnls::{nnls, nnls_core};

use std::sync::LazyLock;

use anyhow::Result;
use dashmap::DashMap;
use oklab::Oklab;

use crate::global;
use crate::color::conversion::rgb_to_oklab;
use crate::config::{ColorAxesSettings, MoodEndpoint};
use crate::db::MoodRow;
use crate::global::Embedder;
use crate::utils::Percentage;

/// State for a single basis mood ray.
#[derive(Debug, Clone)]
pub struct BasisMood {
    pub mood: String,
    pub oklab: Oklab,
    pub vector: Vec<f32>,
}

/// Precomputed state for mood-color regression & blending.
#[derive(Debug, Clone)]
pub struct ColorAxes {
    pub basis_moods: Vec<BasisMood>,
    pub base_vector: Vec<f32>,
    pub steepness: f32,
    pub min_contribution: Percentage,
    pub top_k: usize,
    pub baseline_oklab_l: Percentage,
    /// Gate P on emotional saliency: effective saliency Seff = 1 + P*(S - 1).
    pub emotional_saliency_gate: Percentage,
    /// Text anchor prefixed to a mood before embedding ("person says: "), so
    /// the embedding encodes the mood as a statement.
    pub prefix_string: String,
    /// Text used as the neutral baseline anchor subtracted when computing basis ray shift vectors.
    pub base_string: String,
    /// Precomputed Gram matrix (A^T A) of dot products between basis mood vectors.
    pub gram_matrix: Vec<Vec<f32>>,
}

/// NNLS regression output for one embedding: the contributing basis moods
/// with their raw NNLS weights, the rescaled weights used for blending, and
/// the predicted emotional saliency of the mood text.
#[derive(Debug)]
pub struct MoodWeights {
    /// (basis mood index, raw NNLS weight), filtered by `min_contribution`,
    /// sorted descending, truncated to `top_k` — same order as [`Self::rescaled`].
    pub raw: Vec<(usize, f32)>,
    /// Power-weighted rescale of `raw`, normalized to sum 1.
    pub rescaled: Vec<f32>,
    /// Predicted emotional saliency S in [0, 1] for the mood text (1.0 when
    /// the text is empty or the prediction fails).
    pub saliency: f32,
}

/// Predict the emotional saliency score for un-prefixed raw mood text,
/// falling back to 1.0 on any failure (embedding, session run, extraction).
/// Shared by [`ColorAxes::regression_weights`] and entry creation
/// (`handle_entry` computes the score at insert time).
pub fn predict_saliency(embedder: &Embedder, mood_text: &str) -> f32 {
    let trimmed_text = mood_text.trim();
    if trimmed_text.is_empty() {
        return 1.0;
    }
    match embedder
        .embed(trimmed_text, "")
        .and_then(|raw_emb| embedder.predict_saliency(&raw_emb))
    {
        Ok(s) => s,
        Err(err) => {
            log::error!("Saliency prediction failed for {:?}: {err:#}", trimmed_text);
            1.0
        }
    }
}

impl ColorAxes {
    /// Load and build color axes from the database and mood configuration.
    pub async fn build(pool: &sqlx::SqlitePool, moods: &crate::config::MoodConfig) -> Result<Self> {
        let embedder = crate::global::embedder_async().await;
        let pairs = moods.load_pairs();
        Self::build_inner(pool, embedder, &moods.axes, &pairs).await
    }

    /// Build basis vectors from the given endpoint pairs using SQLite cached
    /// embeddings.
    async fn build_inner(
        pool: &sqlx::SqlitePool,
        embedder: &Embedder,
        settings: &ColorAxesSettings,
        pairs: &[MoodEndpoint],
    ) -> Result<Self> {
        assert!(!pairs.is_empty());

        let v_base =
            global::get_or_embed_cached(pool, embedder, &settings.base_string, "")
                .await?;

        let mut basis_moods = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let s = global::get_or_embed_cached(
                pool,
                embedder,
                &pair.mood,
                &settings.prefix_string,
            )
            .await?;
            let diff: Vec<f32> = s.iter().zip(&v_base).map(|(x, y)| x - y).collect();
            let norm_vector = global::normalize(&diff);
            let oklab = rgb_to_oklab(pair.color);
            basis_moods.push(BasisMood {
                mood: pair.mood.clone(),
                oklab,
                vector: norm_vector,
            });
        }

        let n = basis_moods.len();
        let mut gram_matrix = vec![vec![0.0_f32; n]; n];
        for i in 0..n {
            for j in 0..n {
                gram_matrix[i][j] =
                    global::dot(&basis_moods[i].vector, &basis_moods[j].vector);
            }
        }

        Ok(Self {
            basis_moods,
            base_vector: v_base,
            steepness: settings.blend_steepness.max(1.0),
            min_contribution: settings.min_contribution,
            top_k: settings.top_k,
            baseline_oklab_l: settings.baseline_oklab_l,
            emotional_saliency_gate: settings.effective_saliency_gate,
            prefix_string: settings.prefix_string.clone(),
            base_string: settings.base_string.clone(),
            gram_matrix,
        })
    }

    /// Run the NNLS regression, weight-rescaling, and saliency calculation
    /// stages of the pipeline for `embedding`, returning the contributing
    /// basis moods with their raw NNLS weights, rescaled weights, and the
    /// predicted emotional saliency.
    ///
    /// Returns `None` when the pipeline falls through to the neutral color:
    /// no basis moods, a zero-length target vector, a zero total NNLS weight,
    /// or no basis mood surviving the `min_contribution` filter.
    pub fn regression_weights(
        &self,
        embedding: &[f32],
        embedder: &Embedder,
        saliency: Result<f32, &str>,
    ) -> Option<MoodWeights> {
        let n = self.basis_moods.len();
        if n == 0 || embedding.len() != self.base_vector.len() {
            return None;
        }

        // 1. Compute shift vector length relative to base embedding without heap allocation
        let mut len_x_sq = 0.0_f32;
        for (&x, &b) in embedding.iter().zip(&self.base_vector) {
            let diff = x - b;
            len_x_sq += diff * diff;
        }
        let len_x_norm = len_x_sq.sqrt();
        if len_x_norm < 1e-6 {
            return None;
        }
        let inv_norm = 1.0 / len_x_norm;

        // 2. Compute at_b = A^T * target_vec directly without vector allocation
        let mut at_b = vec![0.0_f32; n];
        for (i, bm) in self.basis_moods.iter().enumerate() {
            let mut dot_sum = 0.0_f32;
            for ((&x, &b), &v) in embedding.iter().zip(&self.base_vector).zip(&bm.vector) {
                dot_sum += (x - b) * v;
            }
            at_b[i] = dot_sum * inv_norm;
        }

        // 3. Run NNLS on precomputed Gram matrix and at_b
        let weights = nnls_core(&self.gram_matrix, &at_b, 300);

        let total_weight: f32 = weights.iter().sum();
        if total_weight < 1e-6 {
            return None;
        }

        // 4. Filter out weights by contribution % < min_contribution
        let min_contrib_thresh = self.min_contribution.to_float();
        let mut raw: Vec<(usize, f32)> = weights
            .iter()
            .enumerate()
            .filter_map(|(i, &w)| {
                if w / total_weight >= min_contrib_thresh {
                    Some((i, w))
                } else {
                    None
                }
            })
            .collect();

        if raw.is_empty() {
            return None;
        }

        // Sort descending by weight and keep top_k
        raw.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if self.top_k > 0 && raw.len() > self.top_k {
            raw.truncate(self.top_k);
        }

        // 5. Compute rescaled weights using blend_steepness
        let max_w = raw.iter().fold(0.0_f32, |acc, (_, w)| acc.max(*w));

        let rescaled: Vec<f32> = if max_w < 1e-6 {
            vec![1.0 / raw.len() as f32; raw.len()]
        } else {
            let inv_max_w = 1.0 / max_w;
            let mut sum_u = 0.0_f32;
            let unnorm: Vec<f32> = raw
                .iter()
                .map(|(_, w)| {
                    let u = (w * inv_max_w).powf(self.steepness);
                    sum_u += u;
                    u
                })
                .collect();
            if sum_u > 0.0 {
                let inv_sum = 1.0 / sum_u;
                unnorm.into_iter().map(|u| u * inv_sum).collect()
            } else {
                vec![1.0 / raw.len() as f32; raw.len()]
            }
        };

        // 6. Emotional saliency: a caller-supplied override (`Ok(score)`) skips
        // the prediction; otherwise predict from the un-prefixed raw text
        // (`Err(mood_text)`, see [`predict_saliency`]).
        let saliency = match saliency {
            Ok(s) => s,
            Err(mood_text) => predict_saliency(embedder, mood_text),
        };

        Some(MoodWeights {
            raw,
            rescaled,
            saliency,
        })
    }

    /// Compute the final Oklab color from a [`MoodWeights`] regression
    /// result (produced by [`Self::regression_weights`]); `None` (the
    /// pipeline fell through) maps to the neutral baseline color.
    pub fn weights_to_color(&self, reg: Option<&MoodWeights>) -> Oklab {
        let l_neutral = self.baseline_oklab_l.to_float();
        let Some(reg) = reg else {
            return Oklab {
                l: l_neutral,
                a: 0.0,
                b: 0.0,
            };
        };

        // 5. Blend Oklab colors using rescaled weights
        let mut blended_l = 0.0;
        let mut blended_a = 0.0;
        let mut blended_b = 0.0;

        for ((idx, _), rw) in reg.raw.iter().zip(&reg.rescaled) {
            let color = self.basis_moods[*idx].oklab;
            blended_l += color.l * rw;
            blended_a += color.a * rw;
            blended_b += color.b * rw;
        }

        // 6. Saliency S is already computed for `mood_text` in
        //    `regression_weights`.
        let saliency = reg.saliency;

        // 7. Gate saliency: Seff = 1 + P*(S - 1), linearly interpolating raw
        //    saliency toward 1.0 so P=100 keeps S unchanged and P=0 disables it.
        let s_eff = self.effective_saliency(saliency);

        // 8. Apply formula:
        // L_final = L_neutral + Seff * (L_blended - L_neutral)
        // a_final = Seff * a_blended
        // b_final = Seff * b_blended
        let l_final = l_neutral + s_eff * (blended_l - l_neutral);
        let a_final = s_eff * blended_a;
        let b_final = s_eff * blended_b;

        Oklab {
            l: l_final,
            a: a_final,
            b: b_final,
        }
    }

    /// Effective saliency after the emotional gate, `Seff = 1 + P*(S - 1)`,
    /// using this axes' configured `emotional_saliency_gate` P.
    pub fn effective_saliency(&self, saliency: f32) -> f32 {
        gated_saliency(saliency, self.emotional_saliency_gate.to_float())
    }

    /// Resolve a mood row to its final Oklab color (pure computation, no cache).
    ///
    /// Sync and backfill-free: rows without a stored embedding are embedded
    /// on the fly (no DB write), and rows without a cached saliency score
    /// fall back to predicting it inline.
    ///
    /// Returns `None` for empty moods or when embedding fails.
    pub fn compute_mood_color(&self, embedder: &Embedder, row: &MoodRow) -> Option<Oklab> {
        let mood = &row.mood;
        if mood.is_empty() {
            return None;
        }
        let embedding = match row
            .embedding
            .as_deref()
            .and_then(global::blob_to_embedding)
        {
            Some(emb) => emb,
            None => match embedder.embed(mood, &self.prefix_string) {
                Ok(emb) => emb,
                Err(_) => return None,
            },
        };
        // The cached score (when present) skips the saliency ONNX pass.
        let reg = self.regression_weights(&embedding, embedder, row.score.ok_or(mood.as_str()));
        Some(self.weights_to_color(reg.as_ref()))
    }
}

/// Process-wide mood-color cache: the single source of truth shared by the
/// today-view render path, the background color fill, and the preview
/// builders. Read-only lookups are pure `get`s; only background tasks (and
/// the one-shot CLI) run the color pipeline and insert results.
pub static GLOBAL_MOOD_COLOR_CACHE: LazyLock<DashMap<String, Oklab>> =
    LazyLock::new(DashMap::new);

/// The process-wide [`GLOBAL_MOOD_COLOR_CACHE`].
pub fn global_mood_color_cache() -> &'static DashMap<String, Oklab> {
    &GLOBAL_MOOD_COLOR_CACHE
}

/// Cached mood color: pure read, no computation. Render paths use this
/// directly; a miss renders the neutral fallback.
pub fn cached_mood_color(mood: &str) -> Option<Oklab> {
    global_mood_color_cache().get(mood).map(|r| *r)
}

/// Resolve a mood's color via the process-wide cache; on a miss, runs the
/// color pipeline (blocking — background tasks only) and writes the result
/// back directly to the global cache.
pub fn mood_color_with_backfill(axes: Option<&ColorAxes>, row: &MoodRow) -> Option<Oklab> {
    if let Some(oklab) = cached_mood_color(&row.mood) {
        return Some(oklab);
    }
    let axes = axes?;
    let embedder = global::embedder();
    let oklab = axes.compute_mood_color(embedder, row)?;
    global_mood_color_cache().insert(row.mood.clone(), oklab);
    Some(oklab)
}

/// Compute colors for mood rows missing from the process-wide cache, and backfill
/// any unpersisted embeddings and saliency scores to the database.
pub async fn compute_mood_colors_and_backfill(
    pool: Option<&sqlx::SqlitePool>,
    rows: &[MoodRow],
    axes: &ColorAxes,
) -> usize {
    let embedder = global::embedder_async().await;
    let cache = global_mood_color_cache();
    let mut added = 0;

    for row in rows {
        if row.mood.is_empty() {
            continue;
        }

        let cached = cache.contains_key(&row.mood);
        if cached && (pool.is_none() || (row.embedding.is_some() && row.score.is_some())) {
            continue;
        }

        // 1. Resolve embedding (from row blob, or ONNX inference)
        let (embedding, needs_emb_backfill) = match row
            .embedding
            .as_deref()
            .and_then(global::blob_to_embedding)
        {
            Some(emb) => (Some(emb), false),
            None => (embedder.embed(&row.mood, &axes.prefix_string).ok(), true),
        };

        let Some(embedding) = embedding else {
            continue;
        };

        // 2. Resolve saliency score (from row, or ONNX prediction)
        let (score, needs_score_backfill) = match row.score {
            Some(s) => (s, false),
            None => (predict_saliency(embedder, &row.mood), true),
        };

        // 3. Persist missing fields to DB if pool is provided and row has valid id
        if let Some(pool) = pool {
            if row.id > 0 {
                let _ = crate::db::update_mood_embedding_and_score(
                    pool,
                    row.id,
                    needs_emb_backfill
                        .then(|| global::embedding_to_blob(&embedding))
                        .as_deref(),
                    needs_score_backfill.then_some(score),
                )
                .await;
            }
        }

        // 4. Compute color and update in-memory cache directly
        if !cached {
            let reg = axes.regression_weights(&embedding, embedder, Ok(score));
            let oklab = axes.weights_to_color(reg.as_ref());
            cache.insert(row.mood.clone(), oklab);
            added += 1;
        }
    }

    added
}

/// Test hook: clear the process-wide cache so color behavior is
/// deterministic despite test cache pollution in one process.
#[doc(hidden)]
pub fn clear_mood_color_cache() {
    global_mood_color_cache().clear();
}

/// Effective saliency after the emotional gate: `Seff = 1 + P*(S - 1)` for gate
/// fraction P in [0, 1]. P = 1.0 leaves raw saliency untouched; P = 0.0 disables
/// saliency (Seff = 1.0).
fn gated_saliency(saliency: f32, gate: f32) -> f32 {
    1.0 + gate * (saliency - 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nnls_simple() {
        // Solves A x = b where A is identity [1,0], [0,1] and b = [0.5, 0.8]
        let columns = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let b = vec![0.5, 0.8];
        let x = nnls(&columns, &b, 100);
        assert_eq!(x.len(), 2);
        assert!((x[0] - 0.5).abs() < 1e-4);
        assert!((x[1] - 0.8).abs() < 1e-4);
    }

    #[test]
    fn test_gated_saliency() {
        // P = 1.0: raw saliency preserved.
        assert_eq!(gated_saliency(0.5, 1.0), 0.5);
        assert_eq!(gated_saliency(1.0, 1.0), 1.0);
        // P = 0.0: saliency disabled -> always 1.0.
        assert_eq!(gated_saliency(0.3, 0.0), 1.0);
        // P = 0.8 (default): Seff = 1 + 0.8*(S - 1).
        assert!((gated_saliency(0.5, 0.8) - 0.6).abs() < 1e-6);
        assert!((gated_saliency(0.0, 0.8) - 0.2).abs() < 1e-6);
        assert!((gated_saliency(1.0, 0.8) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_nnls_nonnegative_constraint() {
        // Target is negative -> NNLS clamps x to 0
        let columns = vec![vec![1.0, 0.0]];
        let b = vec![-0.5, 0.0];
        let x = nnls(&columns, &b, 100);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0], 0.0);
    }
}
