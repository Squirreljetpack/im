//! `_sync_state` accessors, the local device identity, the event clock and the
//! per-field LWW watermarks.
//!
//! The watermarks are what make replay order-independent: a field is written
//! only when its event beats the stamp recorded for that field, so two devices
//! that edited *different* fields of one row keep both edits, and two that
//! edited the *same* field converge on one value whatever order they replay
//! (§4.1).

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

/// The last event that won a field: LWW on `(timestamp, device_id, event_id)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    pub timestamp: i64,
    pub device_id: String,
    pub event_id: String,
}

impl Stamp {
    pub fn new(timestamp: i64, device_id: &str, event_id: &str) -> Self {
        Self {
            timestamp,
            device_id: device_id.to_string(),
            event_id: event_id.to_string(),
        }
    }
}

/// The watermark field that stands for the entity itself rather than one of
/// its columns: the stamp of the last event that changed anything about it.
pub const ENTITY: &str = "";

/// Whether a candidate event wins over the stamp already recorded.
///
/// The device id breaks same-millisecond ties between devices and the event id
/// breaks what is left, so two machines that share a device id (a copied
/// database) still converge on the same winner.
pub fn wins(candidate: &Stamp, current: &Stamp) -> bool {
    (
        candidate.timestamp,
        candidate.device_id.as_str(),
        candidate.event_id.as_str(),
    ) > (
        current.timestamp,
        current.device_id.as_str(),
        current.event_id.as_str(),
    )
}

impl From<&crate::sync::RemoteEvent> for Stamp {
    fn from(remote: &crate::sync::RemoteEvent) -> Self {
        Self::new(
            remote.event.timestamp,
            &remote.event.device_id.to_string(),
            &remote.event.event_id.to_string(),
        )
    }
}

/// The winning stamp of one field, if any event was applied for it.
pub async fn watermark(
    conn: &mut SqliteConnection,
    entity_id: Id,
    field: &str,
) -> Result<Option<Stamp>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT timestamp, device_id, event_id FROM _sync_watermark
          WHERE entity_id = ? AND field = ?",
    )
    .bind(entity_id)
    .bind(field)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to read the sync watermark")?;
    Ok(row.map(|row| Stamp {
        timestamp: row.get("timestamp"),
        device_id: row.get("device_id"),
        event_id: row.get("event_id"),
    }))
}

/// Record a field event that won. `value` is the JSON of the value the field
/// now holds (`None` for a cleared column, and for the entity row).
pub async fn record(
    conn: &mut SqliteConnection,
    entity_id: Id,
    field: &str,
    stamp: &Stamp,
    value: Option<&serde_json::Value>,
) -> Result<()> {
    let json = value.map(|value| value.to_string());
    sqlx::query(
        "INSERT INTO _sync_watermark (entity_id, field, timestamp, device_id, event_id, value)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(entity_id, field) DO UPDATE SET
             timestamp = excluded.timestamp,
             device_id = excluded.device_id,
             event_id = excluded.event_id,
             value = excluded.value",
    )
    .bind(entity_id)
    .bind(field)
    .bind(stamp.timestamp)
    .bind(&stamp.device_id)
    .bind(&stamp.event_id)
    .bind(json)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("Failed to record the sync watermark of '{field}'"))?;
    Ok(())
}

/// Move a field's stamp without touching the value it holds: a delete wins the
/// field, while the snapshot a resurrection starts from stays intact (§4.2.2).
pub async fn record_stamp_keep_value(
    conn: &mut SqliteConnection,
    entity_id: Id,
    field: &str,
    stamp: &Stamp,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE _sync_watermark SET timestamp = ?, device_id = ?, event_id = ?
          WHERE entity_id = ? AND field = ?",
    )
    .bind(stamp.timestamp)
    .bind(&stamp.device_id)
    .bind(&stamp.event_id)
    .bind(entity_id)
    .bind(field)
    .execute(&mut *conn)
    .await
    .context("Failed to move the sync watermark")?;
    if updated.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO _sync_watermark (entity_id, field, timestamp, device_id, event_id, value)
             VALUES (?, ?, ?, ?, ?, NULL)",
        )
        .bind(entity_id)
        .bind(field)
        .bind(stamp.timestamp)
        .bind(&stamp.device_id)
        .bind(&stamp.event_id)
        .execute(&mut *conn)
        .await
        .context("Failed to record the sync watermark")?;
    }
    Ok(())
}

/// Whether the newest decision still sitting in the outbox for this entity is
/// a delete: the local side of a tombstone conflict (§4.2.2).
pub async fn pending_delete(conn: &mut SqliteConnection, entity_id: Id) -> Result<bool> {
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT payload FROM _sync_events WHERE entity_id = ? AND synced = 0
         ORDER BY version DESC LIMIT 1",
    )
    .bind(entity_id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to inspect the sync outbox")?;
    Ok(payload.is_some_and(|json| json.trim() == "null"))
}

/// Every known field value of an entity, newest winner per field: the snapshot
/// a deleted entity is resurrected from (§4.2.2).
pub async fn values(
    conn: &mut SqliteConnection,
    entity_id: Id,
) -> Result<Vec<(String, serde_json::Value)>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT field, value FROM _sync_watermark
          WHERE entity_id = ? AND field != '' AND value IS NOT NULL",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await
    .context("Failed to read the sync snapshot")?;
    rows.iter()
        .map(|row| {
            let field: String = row.get("field");
            let value: String = row.get("value");
            let value = serde_json::from_str(&value).context("Corrupt sync snapshot value")?;
            Ok((field, value))
        })
        .collect()
}

/// What this device knows about an entity: its kind and whether it is deleted
/// here.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub kind: String,
    pub deleted: bool,
}

/// The entity state, if any event has ever been applied for the id.
pub async fn entity(conn: &mut SqliteConnection, entity_id: Id) -> Result<Option<Entity>> {
    use sqlx::Row;
    let row = sqlx::query("SELECT kind, deleted FROM _sync_entities WHERE entity_id = ?")
        .bind(entity_id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the synced entity")?;
    Ok(row.map(|row| Entity {
        kind: row.get("kind"),
        deleted: row.get::<i32, _>("deleted") != 0,
    }))
}

/// Record what kind of entity an id is and whether it is deleted here.
pub async fn set_entity(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    deleted: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO _sync_entities (entity_id, kind, deleted) VALUES (?, ?, ?)
         ON CONFLICT(entity_id) DO UPDATE SET kind = excluded.kind, deleted = excluded.deleted",
    )
    .bind(entity_id)
    .bind(kind)
    .bind(deleted as i32)
    .execute(&mut *conn)
    .await
    .context("Failed to record the synced entity")?;
    Ok(())
}

/// Whether anything about this entity is still waiting in the outbox: the local
/// decision a remote delete can contradict (§4.2.2).
pub async fn has_pending(conn: &mut SqliteConnection, entity_id: Id) -> Result<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sync_events WHERE entity_id = ? AND synced = 0")
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .context("Failed to inspect the sync outbox")?;
    Ok(count > 0)
}

/// The fields an unsynced *edit* of this entity wrote: the local decisions a
/// remote edit of the same field can contradict (§4.2.1). A creation is the
/// base of the entity rather than a competing edit, so its fields are not
/// decisions to defend field by field.
pub async fn pending_fields(
    conn: &mut SqliteConnection,
    entity_id: Id,
) -> Result<std::collections::HashSet<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM _sync_events WHERE entity_id = ? AND synced = 0",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await
    .context("Failed to inspect the sync outbox")?;
    let mut fields = std::collections::HashSet::new();
    for payload in rows {
        let payload: Option<crate::sync::EntityPayload> =
            serde_json::from_str(&payload).context("Corrupt outbox payload")?;
        if let Some(payload) = payload
            && !matches!(
                payload,
                crate::sync::EntityPayload::TaskCreate(_)
                    | crate::sync::EntityPayload::MoodCreate(_)
                    | crate::sync::EntityPayload::TrackerCreate(_)
            )
        {
            fields.extend(payload.changes().into_iter().map(|c| c.field.to_string()));
        }
    }
    Ok(fields)
}
