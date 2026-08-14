use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

pub async fn prune_embedding_cache(pool: &SqlitePool) -> Result<u64> {
    let rows_affected = sqlx::query("DELETE FROM embedding_cache")
        .execute(pool)
        .await?
        .rows_affected();

    Ok(rows_affected)
}

/// Backfill a mood row's stored embedding.
pub async fn update_mood_embedding(pool: &SqlitePool, id: i64, blob: &[u8]) -> Result<u64> {
    let res = sqlx::query("UPDATE mood SET embedding = ? WHERE id = ?")
        .bind(blob)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update mood embedding")?;
    Ok(res.rows_affected())
}

/// Persist a mood's cached saliency score (backfilled by
/// `ColorAxes::mood_color_cached` on the first render pass).
pub async fn update_mood_score(pool: &SqlitePool, id: i64, score: f32) -> Result<u64> {
    let res = sqlx::query("UPDATE mood SET score = ? WHERE id = ?")
        .bind(score)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update mood score")?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// Embedding cache
// ---------------------------------------------------------------------------

/// Look up a cached embedding BLOB by cache key (prefix + text).
pub async fn get_embedding_cache(pool: &SqlitePool, text: &str) -> Result<Option<Vec<u8>>> {
    let row = sqlx::query("SELECT embedding FROM embedding_cache WHERE text = ?")
        .bind(text)
        .fetch_optional(pool)
        .await
        .context("Failed to query embedding cache")?;
    Ok(row.map(|r| r.get("embedding")))
}

/// Insert or replace a cache entry. Returns affected rows.
pub async fn set_embedding_cache(pool: &SqlitePool, text: &str, blob: &[u8]) -> Result<u64> {
    let res = sqlx::query("INSERT OR REPLACE INTO embedding_cache (text, embedding) VALUES (?, ?)")
        .bind(text)
        .bind(blob)
        .execute(pool)
        .await
        .context("Failed to write embedding cache")?;
    Ok(res.rows_affected())
}
