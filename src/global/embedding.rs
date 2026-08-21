//! Mood embeddings: the nomic-embed-text-v1.5 int8 QDQ ONNX model and its tokenizer are
//! bundled into the binary at build time (`include_bytes!` directly from
//! `assets/model/`; see build.rs). Inference runs via ONNX Runtime (ort) with
//! dynamic sequence lengths — no fixed-shape padding — and returns 768-dim
//! sentence embeddings.
//!
//! ort 2.0's `Session::run` takes `&mut self`, so each model is wrapped in a
//! `Mutex` to share the embedder through `OnceLock`; the app serializes
//! inference anyway, so the lock never contends.
//!
//! Loading is a one-time, infallible-in-practice operation guarded by a
//! `OnceLock`; a failure to load the bundled model is a build/runtime invariant
//! violation and panics (`embedder`).

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

/// Dimensionality of nomic-embed-text-v1.5 sentence embeddings.
pub const EMBED_DIM: usize = 768;
/// Tokenizer truncation cap. The model's native context length is 2048; inputs
/// longer than this are truncated before tokenization.
const MAX_SEQ_LEN: usize = 2048;

static EMBED_ONNX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/model/embed.onnx"
));
static SALIENCY_ONNX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/model/saliency_adaptor.onnx"
));
static TOKENIZER_JSON: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/model/tokenizer.json"
));

/// A loaded embedding model: ort sessions for the embedder and saliency
/// adaptor, plus the WordPiece tokenizer.
pub struct Embedder {
    embed: Mutex<Session>,
    saliency: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl Embedder {
    /// Load the bundled embedding model and tokenizer.
    pub fn load() -> Result<Self> {
        let mut tokenizer = Tokenizer::from_bytes(TOKENIZER_JSON)
            .map_err(|e| anyhow::anyhow!("Failed to load embedded tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LEN,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                direction: tokenizers::TruncationDirection::Right,
                stride: 0,
            }))
            .map_err(|e| anyhow::anyhow!("Failed to set truncation: {e}"))?;

        let embed = Session::builder()
            .context("Failed to build ORT session builder")?
            .commit_from_memory(EMBED_ONNX)
            .context("Failed to load embedded embed.onnx session")?;
        let saliency = Session::builder()
            .context("Failed to build ORT session builder")?
            .commit_from_memory(SALIENCY_ONNX)
            .context("Failed to load embedded saliency_adaptor.onnx session")?;

        Ok(Self {
            embed: Mutex::new(embed),
            saliency: Mutex::new(saliency),
            tokenizer,
        })
    }

    /// Compute the sentence embedding for `text`.
    ///
    /// `text` is trimmed before tokenization. `prepend` is concatenated to
    /// the trimmed text before embedding.
    /// Pass `""` for raw text-mode embedding (e.g. diagnostic `:embed`).
    ///
    /// Inputs longer than `MAX_SEQ_LEN` (2048) tokens are silently truncated;
    /// at ~1500 words this exceeds any realistic mood-journal entry.
    ///
    /// Returns a unit-length (L2-normalized) vector of `EMBED_DIM` floats.
    pub fn embed(&self, text: &str, prepend: &str) -> Result<Vec<f32>> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            // Empty input would be meaningless; return a zero vector.
            return Ok(vec![0.0; EMBED_DIM]);
        }
        let text = format!("{prepend}{trimmed}");

        let enc = self
            .tokenizer
            .encode(text.as_str(), true)
            .map_err(|e| anyhow::anyhow!("Failed to tokenize {:?}: {e}", text.as_str()))?;

        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&m| m as i64).collect();
        let type_ids: Vec<i64> = enc.get_type_ids().iter().map(|&t| t as i64).collect();
        let seq_len = ids.len();

        // Dynamic shape: feed the real token count, no fixed-512 padding.
        let shape = vec![1, seq_len];
        let input_ids = Tensor::from_array((shape.clone(), ids))
            .context("Failed to create input_ids tensor")?;
        let attention_mask = Tensor::from_array((shape.clone(), mask.clone()))
            .context("Failed to create attention_mask tensor")?;
        let token_type_ids = Tensor::from_array((shape, type_ids))
            .context("Failed to create token_type_ids tensor")?;

        let mut session = self
            .embed
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock embed session: {e}"))?;
        let outputs = session
            .run(ort::inputs! {
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
                "token_type_ids" => token_type_ids,
            })
            .context("Failed to run embed ONNX session")?;

        let (out_shape, last_hidden_flat) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .context("Failed to extract embed output tensor")?;
        let hidden_dim = out_shape[2] as usize;
        if out_shape.len() < 3 || hidden_dim == 0 {
            anyhow::bail!("Unexpected embed output shape: {:?}", out_shape);
        }

        // Mean pooling: average the token embeddings for all non-padding positions.
        // nomic-embed-text-v1.5 is trained with mean pooling over the attention mask
        // (model card: `mean_pooling(model_output, attention_mask)`), unlike BGE which
        // uses CLS-token pooling. Padding tokens (mask=0) are excluded.
        let valid_count: f32 = mask.iter().map(|&m| m as f32).sum::<f32>().max(1.0);
        let mut pooled = vec![0.0f32; hidden_dim];
        for (t, &m) in mask.iter().enumerate() {
            if m == 1 {
                let offset = t * hidden_dim;
                for d in 0..hidden_dim {
                    pooled[d] += last_hidden_flat[offset + d];
                }
            }
        }
        for v in &mut pooled {
            *v /= valid_count;
        }

        // L2 normalize
        let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for v in &mut pooled {
                *v /= norm;
            }
        }

        Ok(pooled)
    }

    /// Predict scalar emotional saliency in [0.0, 1.0] for a raw (unprefixed) embedding.
    pub fn predict_saliency(&self, raw_embedding: &[f32]) -> Result<f32> {
        let dim = raw_embedding.len();
        let input = Tensor::from_array((vec![1, dim], raw_embedding.to_vec()))
            .context("Failed to create saliency input tensor")?;
        let mut session = self
            .saliency
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock saliency session: {e}"))?;
        let outputs = session
            .run(ort::inputs! {
                "input" => input,
            })
            .context("Failed to run saliency ONNX session")?;
        let (_shape, val) = outputs["output"]
            .try_extract_tensor::<f32>()
            .context("Failed to extract saliency output tensor")?;
        Ok(val.first().copied().unwrap_or(0.0).clamp(0.0, 1.0))
    }
}

/// Read a raw BLOB stored by [`embed_to_blob`] back into a vector.
pub fn blob_to_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() != EMBED_DIM * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(EMBED_DIM);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(out)
}

/// Serialize an embedding vector into a raw little-endian BLOB for SQLite.
pub fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|v| v.to_le_bytes()).collect()
}

static EMBEDDER: OnceLock<Embedder> = OnceLock::new();

/// Kick off background loading of the embedder via `tokio::task::spawn_blocking`.
pub fn init_embedder_background() {
    tokio::task::spawn_blocking(|| {
        let _ = embedder();
    });
}

/// Asynchronously await the loaded embedding model.
///
/// If loading is already complete, this returns immediately. If loading is in
/// progress, the calling async task yields until the model finishes loading.
pub async fn embedder_async() -> &'static Embedder {
    if let Some(e) = EMBEDDER.get() {
        return e;
    }
    tokio::task::spawn_blocking(|| embedder())
        .await
        .expect("Embedding model spawn_blocking panicked")
}

/// Load the bundled embedding model once and return a reference to it.
///
/// In async/Tokio contexts, prefer `embedder_async()`.
pub fn embedder() -> &'static Embedder {
    EMBEDDER.get_or_init(|| match Embedder::load() {
        Ok(e) => e,
        Err(e) => panic!("Embedding model failed to load: {e:#}"),
    })
}

/// Serialize an embedding vector as a line of space-separated floats.
pub fn format_vector(v: &[f32]) -> String {
    v.iter()
        .map(|x| format!("{x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a line of space-separated floats back into a vector.
pub fn parse_vector(s: &str) -> Result<Vec<f32>> {
    let vals = s
        .split_whitespace()
        .map(|tok| {
            tok.parse::<f32>()
                .with_context(|| format!("Invalid float in vector line: {tok:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if vals.is_empty() {
        anyhow::bail!("Empty vector line");
    }
    Ok(vals)
}

/// Normalize a vector to unit length (no-op on zero vectors).
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

/// Dot product of two equal-length vectors.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Cosine similarity of two equal-length vectors; `None` when either vector
/// is zero-length (the angle is undefined) or the lengths differ.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return None;
    }
    Some(dot(a, b) / (na * nb))
}

/// Query SQLite `embedding_cache` for `text`. On miss, compute `embedder.embed(text, prefix)`
/// and persist the resulting BLOB to `embedding_cache`.
pub async fn get_or_embed_cached(
    pool: &sqlx::SqlitePool,
    embedder: &Embedder,
    text: &str,
    prefix: &str,
) -> Result<Vec<f32>> {
    let key = format!("{prefix}{text}");

    if let Ok(Some(blob)) = crate::db::get_embedding_cache(pool, &key).await
        && let Some(vec) = blob_to_embedding(&blob) {
            return Ok(vec);
        }

    let vec = embedder.embed(text, prefix)?;
    let blob = embedding_to_blob(&vec);

    let _ = crate::db::set_embedding_cache(pool, &key, &blob).await;

    Ok(vec)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip() {
        let v: Vec<f32> = (0..EMBED_DIM).map(|i| i as f32 * 0.5).collect();
        let blob = embedding_to_blob(&v);
        assert_eq!(blob.len(), EMBED_DIM * 4);
        let back = blob_to_embedding(&blob).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn blob_rejects_wrong_len() {
        assert!(blob_to_embedding(&[0u8; 10]).is_none());
        assert!(blob_to_embedding(&[]).is_none());
    }

    #[test]
    fn vector_roundtrip() {
        let v: Vec<f32> = (0..5).map(|i| i as f32 * 0.25).collect();
        let s = format_vector(&v);
        assert_eq!(parse_vector(&s).unwrap(), v);
    }

    #[test]
    fn parse_vector_rejects_garbage() {
        assert!(parse_vector("").is_err());
        assert!(parse_vector("1.0 abc 2.0").is_err());
    }

    #[test]
    fn normalize_unit_length() {
        let v = normalize(&[3.0, 4.0]);
        assert!((v[0].powi(2) + v[1].powi(2) - 1.0).abs() < 1e-5);
        assert_eq!(normalize(&[0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn dot_product() {
        assert_eq!(dot(&[1.0, 2.0], &[3.0, 4.0]), 11.0);
    }

    #[test]
    fn cosine_similarity_parallel() {
        let c = cosine_similarity(&[3.0, 4.0], &[6.0, 8.0]).unwrap();
        assert!((c - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let c = cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).unwrap();
        assert!(c.abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_undefined() {
        // Zero vector / empty / length mismatch are all undefined.
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]).is_none());
        assert!(cosine_similarity(&[1.0, 2.0], &[]).is_none());
        assert!(cosine_similarity(&[1.0, 2.0], &[1.0, 2.0, 3.0]).is_none());
    }
}
