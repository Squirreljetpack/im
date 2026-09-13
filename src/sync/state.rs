//! `_sync_state` accessors, the local device identity, the event clock and
//! the per-entity LWW watermark.
//!
//! The watermark is what makes replay order-independent: an event is applied
//! only when its `(timestamp, device_id)` beats the watermark recorded for
//! that entity, so two devices that edited one field offline converge on the
//! same value no matter which log order they replay (§4.1).

use anyhow::{Context, Result};
use sqlx::SqliteConnection;

use crate::db::Id;

pub const KEY_USER_ID: &str = "user_id";
pub const KEY_DEVICE_ID: &str = "device_id";
pub const KEY_AUTH_TOKEN: &str = "auth_token";
pub const KEY_LAST_SERVER_VERSION: &str = "last_server_version";
const KEY_LAST_EVENT_TIMESTAMP: &str = "last_event_timestamp";

/// Read a `_sync_state` value.
pub async fn get(conn: &mut SqliteConnection, key: &str) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM _sync_state WHERE key = ?")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to read sync state '{key}'"))
}

/// Write a `_sync_state` value.
pub async fn set(conn: &mut SqliteConnection, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO _sync_state (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("Failed to write sync state '{key}'"))?;
    Ok(())
}

/// Delete a `_sync_state` key (logout).
pub async fn remove(conn: &mut SqliteConnection, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM _sync_state WHERE key = ?")
        .bind(key)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("Failed to clear sync state '{key}'"))?;
    Ok(())
}

/// The local device id, created on first use. It is the LWW tie-breaker for
/// the events this device authors, so it must stay stable across runs.
pub async fn device_id(conn: &mut SqliteConnection) -> Result<Id> {
    if let Some(text) = get(&mut *conn, KEY_DEVICE_ID).await?
        && let Ok(id) = Id::parse(&text)
    {
        return Ok(id);
    }
    let id = Id::new();
    set(&mut *conn, KEY_DEVICE_ID, &id.to_string()).await?;
    Ok(id)
}

/// Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// The timestamp for an event this device is authoring: wall-clock
/// milliseconds, but always strictly greater than every event this device has
/// already seen. An edit made on a device with a lagging clock must still win
/// over the events it already replayed, otherwise peers would keep the older
/// winner and diverge.
pub async fn next_event_timestamp(conn: &mut SqliteConnection) -> Result<i64> {
    let last_own: i64 = get(&mut *conn, KEY_LAST_EVENT_TIMESTAMP)
        .await?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let seen: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(timestamp), 0) FROM _sync_watermark")
        .fetch_one(&mut *conn)
        .await
        .context("Failed to read the sync watermark clock")?;
    let timestamp = now_ms().max(last_own.max(seen) + 1);
    set(&mut *conn, KEY_LAST_EVENT_TIMESTAMP, &timestamp.to_string()).await?;
    Ok(timestamp)
}

/// What the watermark remembers about one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct Watermark {
    pub timestamp: i64,
    pub device_id: String,
    /// The newest event applied for the entity was a delete.
    pub deleted: bool,
    /// The newest upsert snapshot seen for the entity, kept across deletes so
    /// a deleted entity can be resurrected (§4.3).
    pub last_payload: Option<String>,
}

/// The watermark of one entity, if any event was ever applied for it.
pub async fn watermark(conn: &mut SqliteConnection, entity_id: Id) -> Result<Option<Watermark>> {
    let row = sqlx::query(
        "SELECT timestamp, device_id, deleted, last_payload FROM _sync_watermark WHERE entity_id = ?",
    )
    .bind(entity_id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to read the sync watermark")?;
    Ok(row.map(|row| {
        use sqlx::Row;
        Watermark {
            timestamp: row.get("timestamp"),
            device_id: row.get("device_id"),
            deleted: row.get::<i32, _>("deleted") != 0,
            last_payload: row.get("last_payload"),
        }
    }))
}

/// Whether an event authored by `device` at `timestamp` wins over `current`.
///
/// LWW on `(timestamp, device_id)`: the device id breaks ties so two events
/// stamped in the same millisecond still resolve identically on every device.
pub fn wins(timestamp: i64, device: &str, current: &Watermark) -> bool {
    (timestamp, device) > (current.timestamp, current.device_id.as_str())
}

/// Record the event that just won for an entity. `payload` is the serialized
/// upsert snapshot (`None` for a delete, which keeps the previous snapshot).
pub async fn record_watermark(
    conn: &mut SqliteConnection,
    entity_id: Id,
    timestamp: i64,
    device: &str,
    payload: Option<&str>,
) -> Result<()> {
    let current = watermark(&mut *conn, entity_id).await?;
    if let Some(current) = &current
        && !wins(timestamp, device, current)
    {
        return Ok(());
    }
    // A delete keeps the newest snapshot so the entity can be resurrected.
    let last_payload = payload
        .map(str::to_string)
        .or_else(|| current.and_then(|w| w.last_payload));
    sqlx::query(
        "INSERT INTO _sync_watermark (entity_id, timestamp, device_id, deleted, last_payload)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(entity_id) DO UPDATE SET
             timestamp = excluded.timestamp,
             device_id = excluded.device_id,
             deleted = excluded.deleted,
             last_payload = excluded.last_payload",
    )
    .bind(entity_id)
    .bind(timestamp)
    .bind(device)
    .bind(payload.is_none() as i32)
    .bind(last_payload)
    .execute(&mut *conn)
    .await
    .context("Failed to record the sync watermark")?;
    Ok(())
}
