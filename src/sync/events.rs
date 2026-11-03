//! Event emission and outbox management: every mutation writes cell rows to
//! `_sync_events` inside the same transaction as the row it describes, so the
//! outbox and the domain tables never disagree.
//!
//! The outbox is coalesced by `(entity_id, field)`: a later offline edit to the
//! same cell updates the pending row in place with a fresh `event_id` and
//! timestamp while preserving its queue position, and a delete drops any
//! pending field cells for that entity before recording the `deleted` existence
//! cell.

use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::db::{EventId, Id, log_offset};

use super::schema::{self, Class};
use super::state;
use super::types::{
    CellEvent, CellValue, EXISTENCE, MoodUpdate, TaskUpdate, TrackerUpdate, completion_field,
    deleted, live,
};

/// Upper bound on a single push batch (`sync-server` refuses more than 1000).
pub(super) const MAX_PUSH: i64 = 1000;

/// Emit the creation of a task.
pub async fn task_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    emit_create(conn, "task", id).await
}

/// Emit an edit of one task field.
pub async fn task_update(conn: &mut SqliteConnection, id: Id, update: TaskUpdate) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "task", field, value).await?;
    Ok(())
}

/// Emit the creation of a mood entry.
pub async fn mood_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    emit_create(conn, "mood", id).await
}

/// Emit an edit of one mood field.
pub async fn mood_update(conn: &mut SqliteConnection, id: Id, update: MoodUpdate) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "mood", field, value).await?;
    Ok(())
}

/// Emit the creation of a tracker entry.
pub async fn tracker_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    emit_create(conn, "tracker", id).await
}

/// Emit an edit of one tracker field.
pub async fn tracker_update(
    conn: &mut SqliteConnection,
    id: Id,
    update: TrackerUpdate,
) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "tracker", field, value).await?;
    Ok(())
}

/// Emit a completion row. A delta is an ordinary one-cell entity: its `count`
/// is signed, and totals are the sum of the rows that survive LWW.
pub async fn completion(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    emit_create(conn, "completion", id).await
}

/// Emit a scheduled task's `count` cell on its own: the row already exists and
/// only its state moved (`1` done, `0` failed, `-1` unmarked).
pub async fn completion_count(conn: &mut SqliteConnection, id: Id, value: i32) -> Result<()> {
    append(
        conn,
        id,
        "completion",
        completion_field::COUNT,
        Some(CellValue::int(value)),
    )
    .await?;
    Ok(())
}

/// Emit the moment a completion row describes, for a row edited in place.
pub async fn completion_time(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query("SELECT time, tz_offset, location FROM todo_completions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read a completion's time")?;
    if let Some(row) = row {
        append(
            conn,
            id,
            "completion",
            completion_field::TIME,
            Some(CellValue::int(row.get::<i64, _>("time"))),
        )
        .await?;
        append(
            conn,
            id,
            "completion",
            completion_field::TZ_OFFSET,
            Some(CellValue::int(log_offset(&row, "tz_offset"))),
        )
        .await?;
        append(
            conn,
            id,
            "completion",
            completion_field::LOCATION,
            Some(CellValue::text(row.get::<String, _>("location"))),
        )
        .await?;
    }
    Ok(())
}

/// Emit a delete, dropping any pending field cells for the entity first.
pub async fn delete(conn: &mut SqliteConnection, id: Id, kind: &str) -> Result<()> {
    sqlx::query("DELETE FROM _sync_events WHERE entity_id = ? AND field != ''")
        .bind(id)
        .execute(&mut *conn)
        .await
        .context("Failed to clear pending field events for a deleted entity")?;
    append(conn, id, kind, EXISTENCE, deleted()).await?;
    Ok(())
}

/// Emit the `live` existence cell and one cell per populated schema field of `id`.
async fn emit_create(conn: &mut SqliteConnection, kind: &str, id: Id) -> Result<()> {
    let Some(cells) = row_cells(&mut *conn, kind, id).await? else {
        return Ok(());
    };
    append(conn, id, kind, EXISTENCE, live()).await?;
    for (field, value) in cells {
        append(&mut *conn, id, kind, field, Some(value)).await?;
    }
    Ok(())
}

/// Upsert one cell into the outbox, stamped and attributed.
async fn append(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    field: &str,
    value: Option<CellValue>,
) -> Result<i64> {
    let device = state::device_id(&mut *conn).await?;
    let timestamp = state::next_stamp(&mut *conn).await?;
    let event_id = EventId::new();
    let json = match &value {
        Some(value) => {
            Some(serde_json::to_string(value).context("Failed to serialize a cell value")?)
        }
        None => None,
    };
    let updated = sqlx::query(
        "UPDATE _sync_events
            SET event_id = ?, device_id = ?, kind = ?, value = ?, timestamp = ?
          WHERE entity_id = ? AND field = ?",
    )
    .bind(event_id)
    .bind(device)
    .bind(kind)
    .bind(&json)
    .bind(timestamp)
    .bind(entity_id)
    .bind(field)
    .execute(&mut *conn)
    .await
    .context("Failed to update a pending sync event")?;

    if updated.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO _sync_events
               (event_id, device_id, entity_id, kind, field, value, timestamp)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event_id)
        .bind(device)
        .bind(entity_id)
        .bind(kind)
        .bind(field)
        .bind(&json)
        .bind(timestamp)
        .execute(&mut *conn)
        .await
        .context("Failed to append a sync event")?;
    }
    Ok(timestamp)
}

/// Read all populated schema cells of a live domain row.
async fn row_cells(
    conn: &mut SqliteConnection,
    kind: &str,
    id: Id,
) -> Result<Option<Vec<(&'static str, CellValue)>>> {
    let Some(spec) = schema::kind(kind) else {
        return Ok(None);
    };
    let mut selected = Vec::new();
    for (name, field) in &spec.fields {
        let column = field.column.as_deref().unwrap_or(name);
        if field.class == Class::Scalar {
            selected.push(format!("typeof({column}) AS \"{column}#kind\""));
            selected.push(format!("CAST({column} AS TEXT) AS \"{column}#text\""));
        } else {
            selected.push(format!("\"{column}\""));
        }
    }
    let sql = format!(
        "SELECT {} FROM \"{}\" WHERE id = ?",
        selected.join(", "),
        spec.table
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to read {kind} {id}"))?;
    let Some(row) = row else { return Ok(None) };

    let mut cells: Vec<(&'static str, CellValue)> = Vec::new();
    for (name, field) in &spec.fields {
        let column = field.column.as_deref().unwrap_or(name);
        let value = match field.class {
            Class::Text => row.get::<Option<String>, _>(column).map(CellValue::Text),
            Class::Int => row.get::<Option<i64>, _>(column).map(CellValue::Int),
            Class::Real => row.get::<Option<f64>, _>(column).map(CellValue::Real),
            Class::Scalar => {
                let kind_key = format!("{column}#kind");
                let text_key = format!("{column}#text");
                let storage: Option<String> = row.get(kind_key.as_str());
                let text: Option<String> = row.get(text_key.as_str());
                match (storage.as_deref(), text) {
                    (Some("integer"), Some(text)) => text.parse().ok().map(CellValue::Int),
                    (Some("real"), Some(text)) => text.parse().ok().map(CellValue::Real),
                    (Some("text"), Some(text)) => Some(CellValue::Text(text)),
                    _ => None,
                }
            }
        };
        if let Some(value) = value {
            cells.push((name.as_str(), value));
        }
    }
    Ok(Some(cells))
}

/// Snapshot the cells a row holds right now as JSON so a rejected edit against
/// a concurrently deleted entity can offer resurrection.
pub(super) async fn row_image(
    conn: &mut SqliteConnection,
    id: Id,
    kind: &str,
) -> Result<Option<String>> {
    let Some(cells) = row_cells(conn, kind, id).await? else {
        return Ok(None);
    };
    let image = serde_json::json!({ "kind": kind, "cells": cells });
    Ok(Some(image.to_string()))
}

/// Read the next batch of unsent cells from the outbox without splitting a
/// multi-cell create across the [`MAX_PUSH`] boundary.
pub(super) async fn outbox(conn: &mut SqliteConnection) -> Result<Vec<CellEvent>> {
    let rows = sqlx::query(
        "SELECT id, event_id, device_id, entity_id, kind, field, value, timestamp
           FROM _sync_events
          ORDER BY id ASC
          LIMIT ?",
    )
    .bind(MAX_PUSH)
    .fetch_all(&mut *conn)
    .await
    .context("Failed to read the sync outbox")?;

    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        let value: Option<String> = row.get("value");
        let value = match value {
            Some(json) => {
                Some(serde_json::from_str(&json).context("Failed to decode an outbox value")?)
            }
            None => None,
        };
        events.push(CellEvent {
            event_id: row.get("event_id"),
            device_id: row.get("device_id"),
            entity_id: row.get("entity_id"),
            kind: row.get("kind"),
            field: row.get("field"),
            value,
            timestamp: row.get("timestamp"),
        });
    }

    if rows.len() as i64 == MAX_PUSH
        && let (Some(last_row), Some(last_entity)) = (rows.last(), events.last().map(|e| e.entity_id))
    {
        let is_split_create = events.iter().any(|event| {
            event.entity_id == last_entity && event.field == EXISTENCE && event.value == live()
        });
        if is_split_create {
            let last_id: i64 = last_row.get("id");
            let continues: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM _sync_events WHERE entity_id = ? AND id > ? LIMIT 1",
            )
            .bind(last_entity)
            .bind(last_id)
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to check outbox continuation")?;
            if continues.is_some() {
                let start = events
                    .iter()
                    .position(|event| event.entity_id == last_entity)
                    .unwrap_or(0);
                if start > 0 {
                    events.truncate(start);
                }
            }
        }
    }

    Ok(events)
}

/// Number of cells waiting in the outbox on this connection.
pub async fn unsynced_in(conn: &mut SqliteConnection) -> Result<usize> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sync_events")
        .fetch_one(&mut *conn)
        .await
        .context("Failed to count the sync outbox")?;
    Ok(count as usize)
}

/// Number of cells waiting in the outbox.
pub async fn unsynced_count(pool: &SqlitePool) -> Result<usize> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    unsynced_in(&mut conn).await
}

/// Whether the outbox holds a pending write to `(entity_id, field)` whose LWW
/// stamp `(timestamp, device_id, event_id)` outranks the incoming remote cell.
pub(super) async fn pending_outranks(
    conn: &mut SqliteConnection,
    entity_id: Id,
    field: &str,
    remote_ts: i64,
    remote_device: Id,
    remote_event: EventId,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT timestamp, device_id, event_id FROM _sync_events
          WHERE entity_id = ? AND field = ?",
    )
    .bind(entity_id)
    .bind(field)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to read pending outbox cell")?;
    let Some(row) = row else {
        return Ok(false);
    };
    let local_ts: i64 = row.get("timestamp");
    let local_device: Id = row.get("device_id");
    let local_event: EventId = row.get("event_id");
    Ok((local_ts, local_device, local_event) > (remote_ts, remote_device, remote_event))
}

/// Re-stamp `batch` relative to `server_now` with fresh `event_id`s and clamp
/// the local stamp watermark after a `clock_ahead` rejection.
pub(super) async fn restamp(
    conn: &mut SqliteConnection,
    batch: &[CellEvent],
    server_now: i64,
) -> Result<()> {
    let mut stamp = server_now;
    for event in batch {
        stamp = stamp.saturating_add(1);
        let fresh = EventId::new();
        sqlx::query("UPDATE _sync_events SET event_id = ?, timestamp = ? WHERE event_id = ?")
            .bind(fresh)
            .bind(stamp)
            .bind(event.event_id)
            .execute(&mut *conn)
            .await
            .context("Failed to restamp an outbox cell")?;
    }
    state::reset_clock(&mut *conn, stamp).await
}

/// Delete a judged batch from the outbox.
pub(super) async fn retire(conn: &mut SqliteConnection, batch: &[CellEvent]) -> Result<()> {
    for event in batch {
        sqlx::query("DELETE FROM _sync_events WHERE event_id = ?")
            .bind(event.event_id)
            .execute(&mut *conn)
            .await
            .context("Failed to retire an outbox cell")?;
    }
    Ok(())
}
