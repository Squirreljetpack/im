//! Event emission: every mutation appends one `_sync_events` row (the outbox)
//! and advances the entity's LWW watermark, inside the mutation's own
//! transaction so a crash can never leave a row change without its event.
//!
//! The emits take a snapshot of the row *after* the mutation, so an event
//! always carries the entity's full current state (§3's `Option<Upsert>`).

use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection};

use crate::db::{EventId, Id};

use super::state;
use super::types::{
    CompletionData, EntityPayload, MoodData, SyncEvent, TaskData, TrackerData, TrackerScore,
};

/// Append an upsert event for a task.
pub(crate) async fn task(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let Some(data) = task_data(conn, id).await? else {
        return Ok(());
    };
    emit(conn, id, Some(EntityPayload::Task(data))).await
}

/// Append an upsert event for a mood.
pub(crate) async fn mood(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let Some(data) = mood_data(conn, id).await? else {
        return Ok(());
    };
    emit(conn, id, Some(EntityPayload::Mood(data))).await
}

/// Append an upsert event for a tracker entry.
pub(crate) async fn tracker(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let Some(data) = tracker_data(conn, id).await? else {
        return Ok(());
    };
    emit(conn, id, Some(EntityPayload::Tracker(data))).await
}

/// Append an upsert event for a completion entry.
pub(crate) async fn completion(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let Some(data) = completion_data(conn, id).await? else {
        return Ok(());
    };
    emit(conn, id, Some(EntityPayload::Completion(data))).await
}

/// Append a delete event for an entity of any kind.
pub(crate) async fn delete(conn: &mut SqliteConnection, entity_id: Id) -> Result<()> {
    emit(conn, entity_id, None).await
}

/// Append an upsert event for an entity that already has its payload at hand
/// (the replayer uses this to re-publish a resurrected entity).
pub(crate) async fn upsert_payload(
    conn: &mut SqliteConnection,
    entity_id: Id,
    payload: EntityPayload,
) -> Result<()> {
    emit(conn, entity_id, Some(payload)).await
}

async fn emit(
    conn: &mut SqliteConnection,
    entity_id: Id,
    payload: Option<EntityPayload>,
) -> Result<()> {
    let device = state::device_id(&mut *conn).await?;
    let timestamp = state::next_event_timestamp(&mut *conn).await?;
    let event_id = EventId::new();
    let json = serde_json::to_string(&payload).context("Failed to serialize a sync event")?;
    sqlx::query(
        "INSERT INTO _sync_events (event_id, device_id, entity_id, timestamp, payload, synced)
         VALUES (?, ?, ?, ?, ?, 0)",
    )
    .bind(event_id)
    .bind(device)
    .bind(entity_id)
    .bind(timestamp)
    .bind(&json)
    .execute(&mut *conn)
    .await
    .context("Failed to append a sync event")?;
    // A delete records no snapshot: the watermark keeps the newest upsert.
    let snapshot = payload.is_some().then_some(json.as_str());
    state::record_watermark(
        &mut *conn,
        entity_id,
        event_id,
        timestamp,
        &device.to_string(),
        snapshot,
    )
    .await
}

/// Serialize an event for the outbox (`None` payload = "null").
pub fn to_json(event: &SyncEvent) -> Result<String> {
    serde_json::to_string(event).context("Failed to serialize a sync event")
}

async fn task_data(conn: &mut SqliteConnection, id: Id) -> Result<Option<TaskData>> {
    let row = sqlx::query(
        "SELECT name, body, priority, start_time, available_duration_secs, interval_secs,
                target_count, optional, end_time, parent
         FROM todos WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to snapshot a task for sync")?;
    Ok(row.map(|row| TaskData {
        name: row.get("name"),
        body: row.get("body"),
        priority: row.get("priority"),
        start_time: row.get("start_time"),
        available_duration_secs: row.get("available_duration_secs"),
        interval_secs: row.get("interval_secs"),
        target_count: row.get("target_count"),
        optional: row.get::<i32, _>("optional") != 0,
        end_time: row.get("end_time"),
        parent_id: row.get("parent"),
    }))
}

async fn mood_data(conn: &mut SqliteConnection, id: Id) -> Result<Option<MoodData>> {
    let row =
        sqlx::query("SELECT mood, body, time, score, duration, todo_id FROM mood WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to snapshot a mood for sync")?;
    Ok(row.map(|row| MoodData {
        mood: row.get("mood"),
        body: row.get("body"),
        time: row.get("time"),
        score: row.get("score"),
        duration: row.get("duration"),
        todo_id: row.get("todo_id"),
    }))
}

async fn tracker_data(conn: &mut SqliteConnection, id: Id) -> Result<Option<TrackerData>> {
    let row = sqlx::query(
        "SELECT type, typeof(score) AS storage, score, time, mood FROM tracker WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to snapshot a tracker for sync")?;
    Ok(row.map(|row| TrackerData {
        tracker_type: row.get("type"),
        score: tracker_score(&row),
        time: row.get("time"),
        mood_id: row.get("mood"),
    }))
}

/// Decode the dynamically typed `tracker.score` column by its storage class.
fn tracker_score(row: &sqlx::sqlite::SqliteRow) -> TrackerScore {
    match row.get::<String, _>("storage").as_str() {
        "integer" => TrackerScore::Integer(row.get("score")),
        "real" => TrackerScore::Float(row.get("score")),
        _ => TrackerScore::Text(row.get("score")),
    }
}

async fn completion_data(conn: &mut SqliteConnection, id: Id) -> Result<Option<CompletionData>> {
    let row = sqlx::query("SELECT todo_id, time, count FROM todo_completions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to snapshot a completion for sync")?;
    Ok(row.map(|row| CompletionData {
        todo_id: row.get("todo_id"),
        time: row.get("time"),
        count: row.get("count"),
    }))
}
