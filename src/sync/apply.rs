//! Replay of incoming events, with the two promptable delete conflicts of
//! §4.3.
//!
//! An event is applied only when it beats the entity's watermark, so replay is
//! order-independent (§4.1). A *delete* that contradicts a local decision still
//! waiting in the outbox is not resolved silently: the caller is handed a
//! [`Conflict`] and decides via [`resolve_conflict`], whose result is itself an
//! event — so every device converges and prompt loops terminate.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use crate::db::Id;

use super::events;
use super::state::{self, Watermark};
use super::types::{EntityPayload, SyncEvent};

/// An event pulled from the server: the log position and the authoring device
/// (the author is the LWW tie-breaker, so it has to travel with the event).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RemoteEvent {
    pub version: i64,
    pub event_id: crate::db::EventId,
    pub device_id: Id,
    #[serde(flatten)]
    pub event: SyncEvent,
}

/// Why an incoming event needs the user's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// A delete arrived for an entity this device edited locally (the local
    /// edit is still unsynced).
    RemoteDeleteVsLocalEdit,
    /// An upsert arrived for an entity this device deleted locally (the
    /// delete is still unsynced).
    RemoteUpsertVsLocalDelete,
    /// A completion arrived for a task this device does not have.
    CompletionOnMissingTask,
}

/// The user's decision on a conflict (the doc's [1]/[2]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// [1] Confirm the deletion: discard the local edit / drop the completion.
    ConfirmRemote,
    /// [2] Resurrect the entity: keep the local side / restore the snapshot.
    Resurrect,
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub entity_id: Id,
    /// The event that triggered the conflict.
    pub event: RemoteEvent,
    /// Whether option [2] is available: the local side (or the incoming
    /// snapshot / watermark) holds enough data to restore the entity.
    pub resurrectable: bool,
}

/// What applying one event did.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    Applied,
    /// The event lost to the entity's watermark — nothing to do.
    Stale,
    /// The caller must ask the user and then call [`resolve_conflict`].
    Conflict(Box<Conflict>),
}

impl ConflictKind {
    pub fn describe(&self) -> &'static str {
        match self {
            ConflictKind::RemoteDeleteVsLocalEdit => {
                "another device deleted an entry this device edited"
            }
            ConflictKind::RemoteUpsertVsLocalDelete => {
                "another device edited an entry this device deleted"
            }
            ConflictKind::CompletionOnMissingTask => {
                "a completion arrived for a task this device does not have"
            }
        }
    }

    pub fn confirm_label(&self) -> &'static str {
        match self {
            ConflictKind::CompletionOnMissingTask => "Drop the completion",
            _ => "Confirm the deletion",
        }
    }

    pub fn resurrect_label(&self) -> &'static str {
        match self {
            ConflictKind::RemoteUpsertVsLocalDelete => "Keep the edit",
            ConflictKind::CompletionOnMissingTask => "Resurrect the task",
            ConflictKind::RemoteDeleteVsLocalEdit => "Resurrect the entry",
        }
    }
}

/// The tables an entity id can live in.
const TABLES: [&str; 4] = ["mood", "tracker", "todos", "todo_completions"];

/// Apply one pulled event.
pub async fn apply_event(pool: &SqlitePool, remote: &RemoteEvent) -> Result<ApplyOutcome> {
    let mut tx = pool.begin().await.context("Failed to begin a sync apply")?;
    let entity_id = remote.event.id;
    let watermark = state::watermark(&mut tx, entity_id).await?;

    if let Some(watermark) = &watermark
        && !state::wins(
            remote.event.timestamp,
            &remote.device_id.to_string(),
            watermark,
        )
    {
        return Ok(ApplyOutcome::Stale);
    }

    let outcome = match &remote.event.payload {
        None => apply_delete(&mut tx, remote, watermark.as_ref()).await?,
        Some(payload) => apply_upsert(&mut tx, remote, payload, watermark.as_ref()).await?,
    };
    if matches!(outcome, ApplyOutcome::Applied) {
        tx.commit().await.context("Failed to commit a sync apply")?;
        // A task that arrived without a local short id gets one (short ids are
        // a local projection and never travel).
        if matches!(remote.event.payload, Some(EntityPayload::Task(_))) {
            crate::db::sync_short_id(pool, entity_id).await?;
        }
    }
    Ok(outcome)
}

async fn apply_delete(
    conn: &mut sqlx::SqliteConnection,
    remote: &RemoteEvent,
    watermark: Option<&Watermark>,
) -> Result<ApplyOutcome> {
    let entity_id = remote.event.id;
    if watermark.is_some_and(|w| !w.deleted) && has_unsynced_events(conn, entity_id).await? {
        return Ok(ApplyOutcome::Conflict(Box::new(Conflict {
            kind: ConflictKind::RemoteDeleteVsLocalEdit,
            entity_id,
            event: remote.clone(),
            resurrectable: true,
        })));
    }

    if let Some(table) = find_entity_table(conn, entity_id).await? {
        let sql = format!("DELETE FROM {table} WHERE id = ?");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(entity_id)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("Failed to delete a synced {table} row"))?;
    }
    state::record_watermark(
        conn,
        entity_id,
        remote.event.timestamp,
        &remote.device_id.to_string(),
        None,
    )
    .await?;
    Ok(ApplyOutcome::Applied)
}

async fn apply_upsert(
    conn: &mut sqlx::SqliteConnection,
    remote: &RemoteEvent,
    payload: &EntityPayload,
    watermark: Option<&Watermark>,
) -> Result<ApplyOutcome> {
    // A local delete still in the outbox is a decision the user must confirm.
    if watermark.is_some_and(|w| w.deleted) && has_unsynced_delete(conn, remote.event.id).await? {
        return Ok(ApplyOutcome::Conflict(Box::new(Conflict {
            kind: ConflictKind::RemoteUpsertVsLocalDelete,
            entity_id: remote.event.id,
            event: remote.clone(),
            resurrectable: true,
        })));
    }

    if let EntityPayload::Completion(data) = payload
        && find_entity_table(conn, data.todo_id).await?.is_none()
    {
        // The task is missing locally: applying the row would break the FK.
        let task_snapshot = state::watermark(conn, data.todo_id)
            .await?
            .and_then(|w| w.last_payload);
        return Ok(ApplyOutcome::Conflict(Box::new(Conflict {
            kind: ConflictKind::CompletionOnMissingTask,
            entity_id: remote.event.id,
            event: remote.clone(),
            resurrectable: task_snapshot.is_some(),
        })));
    }

    write_payload(conn, remote.event.id, payload).await?;
    let json = serde_json::to_string(&Some(payload)).context("Failed to serialize a sync event")?;
    state::record_watermark(
        conn,
        remote.event.id,
        remote.event.timestamp,
        &remote.device_id.to_string(),
        Some(&json),
    )
    .await?;
    Ok(ApplyOutcome::Applied)
}

/// Insert/update the row an upsert payload describes, dropping links whose
/// target no longer exists (deletions take precedence for soft links).
async fn write_payload(
    conn: &mut sqlx::SqliteConnection,
    entity_id: Id,
    payload: &EntityPayload,
) -> Result<()> {
    match payload {
        EntityPayload::Task(data) => {
            let parent = match data.parent_id {
                Some(parent) if find_entity_table(conn, parent).await?.is_some() => Some(parent),
                _ => None,
            };
            sqlx::query(
                "INSERT INTO todos (id, name, body, priority, start_time, available_duration_secs,
                                    interval_secs, target_count, optional, end_time, parent)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     name = excluded.name, body = excluded.body, priority = excluded.priority,
                     start_time = excluded.start_time,
                     available_duration_secs = excluded.available_duration_secs,
                     interval_secs = excluded.interval_secs,
                     target_count = excluded.target_count, optional = excluded.optional,
                     end_time = excluded.end_time, parent = excluded.parent",
            )
            .bind(entity_id)
            .bind(&data.name)
            .bind(&data.body)
            .bind(data.priority)
            .bind(data.start_time)
            .bind(data.available_duration_secs)
            .bind(data.interval_secs)
            .bind(data.target_count)
            .bind(i32::from(data.optional))
            .bind(data.end_time)
            .bind(parent)
            .execute(&mut *conn)
            .await
            .context("Failed to apply a synced task")?;
        }
        EntityPayload::Mood(data) => {
            let todo_id = match data.todo_id {
                Some(todo) if find_entity_table(conn, todo).await?.is_some() => Some(todo),
                _ => None,
            };
            // `embedding` is client-local: a synced mood re-embeds on demand.
            sqlx::query(
                "INSERT INTO mood (id, mood, body, time, score, duration, todo_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     mood = excluded.mood, body = excluded.body, time = excluded.time,
                     score = excluded.score, duration = excluded.duration,
                     todo_id = excluded.todo_id",
            )
            .bind(entity_id)
            .bind(&data.mood)
            .bind(&data.body)
            .bind(data.time)
            .bind(data.score)
            .bind(data.duration)
            .bind(todo_id)
            .execute(&mut *conn)
            .await
            .context("Failed to apply a synced mood")?;
        }
        EntityPayload::Tracker(data) => {
            let mood = match data.mood_id {
                Some(mood) if find_entity_table(conn, mood).await?.is_some() => Some(mood),
                _ => None,
            };
            let mut query = sqlx::query(
                "INSERT INTO tracker (id, type, score, time, mood) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     type = excluded.type, score = excluded.score, time = excluded.time,
                     mood = excluded.mood",
            )
            .bind(entity_id)
            .bind(&data.tracker_type);
            query = match &data.score {
                super::types::TrackerScore::Text(text) => query.bind(text.as_str()),
                super::types::TrackerScore::Integer(number) => query.bind(*number),
                super::types::TrackerScore::Float(float) => query.bind(*float),
            };
            query
                .bind(data.time)
                .bind(mood)
                .execute(&mut *conn)
                .await
                .context("Failed to apply a synced tracker entry")?;
        }
        EntityPayload::Completion(data) => {
            sqlx::query(
                "INSERT INTO todo_completions (id, todo_id, time, count) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     todo_id = excluded.todo_id, time = excluded.time, count = excluded.count",
            )
            .bind(entity_id)
            .bind(data.todo_id)
            .bind(data.time)
            .bind(data.count)
            .execute(&mut *conn)
            .await
            .context("Failed to apply a synced completion")?;
        }
    }
    Ok(())
}

/// Resolve a conflict the user was prompted for. The decision is written as an
/// event so every device converges on it.
pub async fn resolve_conflict(
    pool: &SqlitePool,
    conflict: &Conflict,
    resolution: Resolution,
) -> Result<()> {
    let entity_id = conflict.entity_id;
    let mut tx = pool.begin().await.context("Failed to begin a resolution")?;

    // Acknowledge the incoming event first. Besides recording what we have
    // seen, this sets the timestamp floor for the compensating event we are
    // about to author: our decision has to outrank the event it overrules on
    // every device, even when that event's clock ran ahead of ours.
    let snapshot = conflict
        .event
        .event
        .payload
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize the acknowledged event")?;
    state::record_watermark(
        &mut tx,
        entity_id,
        conflict.event.event.timestamp,
        &conflict.event.device_id.to_string(),
        snapshot.as_deref(),
    )
    .await?;

    match (conflict.kind, resolution) {
        (ConflictKind::RemoteDeleteVsLocalEdit, Resolution::ConfirmRemote) => {
            // The deletion wins: drop the local edit and apply the delete.
            if let Some(table) = find_entity_table(&mut tx, entity_id).await? {
                let sql = format!("DELETE FROM {table} WHERE id = ?");
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(entity_id)
                    .execute(&mut *tx)
                    .await
                    .context("Failed to apply the confirmed deletion")?;
            }
            mark_events_synced(&mut tx, entity_id).await?;
        }
        (ConflictKind::RemoteDeleteVsLocalEdit, Resolution::Resurrect) => {
            // Keep the local entry and re-publish it so it wins LWW everywhere.
            republish_local(&mut tx, entity_id).await?;
        }
        (ConflictKind::RemoteUpsertVsLocalDelete, Resolution::ConfirmRemote) => {
            // Keep the deletion and re-publish it with a fresh timestamp.
            mark_events_synced(&mut tx, entity_id).await?;
            events::delete(&mut tx, entity_id).await?;
        }
        (ConflictKind::RemoteUpsertVsLocalDelete, Resolution::Resurrect) => {
            // Accept the incoming state, then re-publish it with a fresh
            // timestamp so it beats the local (and any other) deletion.
            mark_events_synced(&mut tx, entity_id).await?;
            if let Some(payload) = &conflict.event.event.payload {
                write_payload(&mut tx, entity_id, payload).await?;
                events::upsert_payload(&mut tx, entity_id, payload.clone()).await?;
            }
        }
        (ConflictKind::CompletionOnMissingTask, Resolution::ConfirmRemote) => {
            // Drop the completion: an explicit delete event converges every
            // device (the row exists nowhere locally yet).
            mark_events_synced(&mut tx, entity_id).await?;
            events::delete(&mut tx, entity_id).await?;
        }
        (ConflictKind::CompletionOnMissingTask, Resolution::Resurrect) => {
            let Some(EntityPayload::Completion(data)) = &conflict.event.event.payload else {
                anyhow::bail!("a completion conflict must carry a completion payload");
            };
            let todo_id = data.todo_id;
            let snapshot = state::watermark(&mut tx, todo_id)
                .await?
                .and_then(|w| w.last_payload)
                .context("no snapshot of the deleted task is available")?;
            let payload: Option<EntityPayload> =
                serde_json::from_str(&snapshot).context("Corrupt task snapshot")?;
            let Some(payload) = payload else {
                anyhow::bail!("the stored task snapshot is a deletion");
            };
            // Restore the task, publish the resurrection, then keep the
            // completion the other device logged against it.
            write_payload(&mut tx, todo_id, &payload).await?;
            events::upsert_payload(&mut tx, todo_id, payload).await?;
            write_payload(
                &mut tx,
                entity_id,
                &conflict.event.event.payload.clone().unwrap(),
            )
            .await?;
        }
    }
    tx.commit().await.context("Failed to commit a resolution")?;
    if matches!(conflict.kind, ConflictKind::CompletionOnMissingTask)
        && matches!(resolution, Resolution::Resurrect)
        && let Some(EntityPayload::Completion(data)) = &conflict.event.event.payload
    {
        crate::db::sync_short_id(pool, data.todo_id).await?;
    }
    Ok(())
}

/// Re-publish the local row of an entity with a fresh timestamp.
async fn republish_local(conn: &mut sqlx::SqliteConnection, entity_id: Id) -> Result<()> {
    match find_entity_table(conn, entity_id).await? {
        Some("todos") => events::task(conn, entity_id).await,
        Some("mood") => events::mood(conn, entity_id).await,
        Some("tracker") => events::tracker(conn, entity_id).await,
        Some("todo_completions") => events::completion(conn, entity_id).await,
        Some(other) => anyhow::bail!("unexpected entity table '{other}'"),
        None => Ok(()),
    }
}

/// The events still sitting in the outbox for an entity.
async fn has_unsynced_events(conn: &mut sqlx::SqliteConnection, entity_id: Id) -> Result<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sync_events WHERE entity_id = ? AND synced = 0")
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .context("Failed to inspect the sync outbox")?;
    Ok(count > 0)
}

/// Whether the outbox's latest decision for an entity is a delete.
async fn has_unsynced_delete(conn: &mut sqlx::SqliteConnection, entity_id: Id) -> Result<bool> {
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

/// Stop pushing the queued events of an entity: the user's decision
/// superseded them.
async fn mark_events_synced(conn: &mut sqlx::SqliteConnection, entity_id: Id) -> Result<()> {
    sqlx::query("UPDATE _sync_events SET synced = 1 WHERE entity_id = ? AND synced = 0")
        .bind(entity_id)
        .execute(&mut *conn)
        .await
        .context("Failed to settle the sync outbox")?;
    Ok(())
}

/// Which domain table holds `id` (a delete event carries only the id).
async fn find_entity_table(
    conn: &mut sqlx::SqliteConnection,
    id: Id,
) -> Result<Option<&'static str>> {
    for table in TABLES {
        let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?)");
        let found: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("Failed to look up id in {table}"))?;
        if found {
            return Ok(Some(table));
        }
    }
    Ok(None)
}

/// The outbox rows to push, oldest first: `(event_id, SyncEvent)`.
pub async fn pending_events(pool: &SqlitePool) -> Result<Vec<(crate::db::EventId, SyncEvent)>> {
    let rows = sqlx::query(
        "SELECT event_id, entity_id, timestamp, payload FROM _sync_events
         WHERE synced = 0 ORDER BY version ASC",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read the sync outbox")?;
    rows.iter()
        .map(|row| {
            let payload: Option<EntityPayload> =
                serde_json::from_str(row.get("payload")).context("Corrupt outbox payload")?;
            Ok((
                row.get("event_id"),
                SyncEvent {
                    id: row.get("entity_id"),
                    timestamp: row.get("timestamp"),
                    payload,
                },
            ))
        })
        .collect()
}

/// Mark the outbox rows as pushed (their server ack arrived).
pub async fn mark_pushed(pool: &SqlitePool, event_ids: &[crate::db::EventId]) -> Result<()> {
    for event_id in event_ids {
        sqlx::query("UPDATE _sync_events SET synced = 1 WHERE event_id = ?")
            .bind(event_id)
            .execute(pool)
            .await
            .context("Failed to settle a pushed event")?;
    }
    Ok(())
}
