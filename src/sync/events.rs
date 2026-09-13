//! Event emission: a mutation appends its outbox row and records the field
//! watermarks its event won (`@@SYNC.md` §2.1, §4.1).
//!
//! Every mutation of the domain tables goes through here inside the same
//! transaction as the row it describes, so the event stream and the
//! materialized tables never disagree.

use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection};

use crate::db::{EventId, Id};

use super::state::{self, Stamp};
use super::types::{
    Change, CompletionData, EntityPayload, MoodCreateData, MoodUpdateData, TaskCreateData,
    TaskUpdateData, TrackerData, TrackerScore, TrackerUpdateData,
};

/// Emit the creation snapshot of a task.
pub async fn task_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query(
        "SELECT name, body, priority, start_time, available_duration_secs, interval_secs,
                target_count, optional, end_time, parent
         FROM todos WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to read the task for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    emit_mutation(
        conn,
        id,
        EntityPayload::TaskCreate(TaskCreateData {
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
        }),
    )
    .await
}

/// Emit an edit of a task: only the fields the edit changed.
pub async fn task_update(conn: &mut SqliteConnection, id: Id, diff: TaskUpdateData) -> Result<()> {
    if diff.is_empty() {
        return Ok(());
    }
    emit_mutation(conn, id, EntityPayload::TaskUpdate(diff)).await
}

/// Emit the creation snapshot of a mood entry.
pub async fn mood_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row =
        sqlx::query("SELECT mood, body, time, score, duration, todo_id FROM mood WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to read the mood for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    emit_mutation(
        conn,
        id,
        EntityPayload::MoodCreate(MoodCreateData {
            mood: row.get("mood"),
            body: row.get("body"),
            time: row.get("time"),
            score: row.get("score"),
            duration: row.get("duration"),
            todo_id: row.get("todo_id"),
        }),
    )
    .await
}

/// Emit an edit of a mood entry: only the fields the edit changed.
pub async fn mood_update(conn: &mut SqliteConnection, id: Id, diff: MoodUpdateData) -> Result<()> {
    if diff.is_empty() {
        return Ok(());
    }
    emit_mutation(conn, id, EntityPayload::MoodUpdate(diff)).await
}

/// Emit the creation snapshot of a tracker entry.
pub async fn tracker_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query(
        "SELECT type, typeof(score) AS score_kind, CAST(score AS TEXT) AS score_text, time, mood
         FROM tracker WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to read the tracker for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    let score =
        crate::db::tracker_value(row.get("score_kind"), &row.get::<String, _>("score_text"));
    emit_mutation(
        conn,
        id,
        EntityPayload::TrackerCreate(TrackerData {
            tracker_type: row.get("type"),
            score: TrackerScore::from(&score),
            time: row.get("time"),
            mood_id: row.get("mood"),
        }),
    )
    .await
}

/// Emit an edit of a tracker entry: only the fields the edit changed.
pub async fn tracker_update(
    conn: &mut SqliteConnection,
    id: Id,
    diff: TrackerUpdateData,
) -> Result<()> {
    if diff.is_empty() {
        return Ok(());
    }
    emit_mutation(conn, id, EntityPayload::TrackerUpdate(diff)).await
}

/// Emit a logged completion (append-only, never edited — §3).
pub async fn completion(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query("SELECT todo_id, time, count FROM todo_completions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the completion for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    emit_mutation(
        conn,
        id,
        EntityPayload::Completion(CompletionData {
            todo_id: row.get("todo_id"),
            time: row.get("time"),
            count: row.get("count"),
        }),
    )
    .await
}

/// Emit a delete: the entity loses every field, so a later mutation has to
/// outrank the deletion to resurrect it (§4.2.2).
pub async fn delete(conn: &mut SqliteConnection, id: Id, kind: &str) -> Result<()> {
    let stamp = append(conn, id, &None).await?;
    for field in super::types::fields(kind) {
        state::record_stamp_keep_value(&mut *conn, id, field, &stamp).await?;
    }
    state::record(&mut *conn, id, state::ENTITY, &stamp, None).await?;
    state::set_entity(&mut *conn, id, kind, true).await?;
    Ok(())
}

/// Publish one field with a fresh stamp: the compensating event of a conflict
/// the user settled in this field's favour (§4.2).
pub async fn republish_field(
    conn: &mut SqliteConnection,
    id: Id,
    kind: &str,
    change: &Change,
) -> Result<()> {
    let payload = super::types::payload_of_change(kind, change)?;
    emit_mutation(conn, id, payload).await
}

/// Re-publish an entity's current values: the compensating event of a
/// resurrection (§4.2.2), which has to outrank the deletion it overrules.
pub async fn republish(conn: &mut SqliteConnection, id: Id, kind: &str) -> Result<()> {
    match kind {
        "task" => task_create(conn, id).await,
        "mood" => mood_create(conn, id).await,
        "tracker" => tracker_create(conn, id).await,
        "completion" => completion(conn, id).await,
        other => anyhow::bail!("cannot re-publish an entity of kind '{other}'"),
    }
}

/// Append one event to the outbox: the device id, the monotonic timestamp and
/// the id the LWW order ties on.
async fn append(
    conn: &mut SqliteConnection,
    entity_id: Id,
    payload: &Option<EntityPayload>,
) -> Result<Stamp> {
    let device = state::device_id(&mut *conn).await?;
    let timestamp = state::next_event_timestamp(&mut *conn).await?;
    let event_id = EventId::new();
    let json = serde_json::to_string(payload).context("Failed to serialize a sync event")?;
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
    Ok(Stamp::new(
        timestamp,
        &device.to_string(),
        &event_id.to_string(),
    ))
}

/// Append a mutation and record the fields it won for this device.
async fn emit_mutation(
    conn: &mut SqliteConnection,
    entity_id: Id,
    payload: EntityPayload,
) -> Result<()> {
    let changes = payload.changes();
    let kind = payload.kind();
    let stamp = append(conn, entity_id, &Some(payload)).await?;
    for change in changes {
        state::record(
            &mut *conn,
            entity_id,
            change.field,
            &stamp,
            change.value.as_ref(),
        )
        .await?;
    }
    state::record(&mut *conn, entity_id, state::ENTITY, &stamp, None).await?;
    state::set_entity(&mut *conn, entity_id, kind, false).await?;
    Ok(())
}
