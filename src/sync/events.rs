//! Event emission: every mutation appends cell rows to the outbox inside the
//! same transaction as the row it describes, so the log and the tables never
//! disagree.
//!
//! A create is the existence cell plus one cell per populated field; an edit is
//! one cell; a delete is the existence cell written `deleted`, and it carries a
//! snapshot of the row so the user can be offered a copy afterwards.

use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection};

use crate::db::{EventId, Id};

use super::schema::{self, Class};
use super::state;
use super::types::{
    CellValue, MoodUpdate, TaskUpdate, TrackerUpdate, completion_field, deleted, live, mood_field,
    task_field, tracker_field,
};

/// Emit the creation of a task.
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
    let cells = [
        (
            task_field::NAME,
            Some(CellValue::text(row.get::<String, _>("name"))),
        ),
        (
            task_field::BODY,
            Some(CellValue::text(row.get::<String, _>("body"))),
        ),
        (
            task_field::PRIORITY,
            Some(CellValue::int(row.get::<i32, _>("priority"))),
        ),
        (
            task_field::START_TIME,
            row.get::<Option<i64>, _>("start_time").map(CellValue::Int),
        ),
        (
            task_field::AVAILABLE_DURATION_SECS,
            row.get::<Option<i64>, _>("available_duration_secs")
                .map(CellValue::Int),
        ),
        (
            task_field::INTERVAL_SECS,
            row.get::<Option<i64>, _>("interval_secs").map(CellValue::Int),
        ),
        (
            task_field::TARGET_COUNT,
            Some(CellValue::int(row.get::<i32, _>("target_count"))),
        ),
        (
            task_field::OPTIONAL,
            Some(CellValue::int(row.get::<i32, _>("optional"))),
        ),
        (
            task_field::END_TIME,
            row.get::<Option<i64>, _>("end_time").map(CellValue::Int),
        ),
        (
            task_field::PARENT_ID,
            row.get::<Option<Id>, _>("parent")
                .map(|parent| CellValue::text(parent.to_string())),
        ),
    ];
    create(conn, id, "task", &cells).await
}

/// Emit an edit of one task field.
pub async fn task_update(conn: &mut SqliteConnection, id: Id, update: TaskUpdate) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "task", field, value, None).await?;
    Ok(())
}

/// Emit the creation of a mood entry.
pub async fn mood_create(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query("SELECT mood, body, time, duration, todo_id FROM mood WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the mood for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    // `score` is not here on purpose: it is a cache of a function of `body`,
    // so it is derived on read rather than replicated.
    let cells = [
        (
            mood_field::MOOD,
            Some(CellValue::text(row.get::<String, _>("mood"))),
        ),
        (
            mood_field::BODY,
            Some(CellValue::text(row.get::<String, _>("body"))),
        ),
        (
            mood_field::TIME,
            Some(CellValue::int(row.get::<i64, _>("time"))),
        ),
        (
            mood_field::DURATION,
            row.get::<Option<i64>, _>("duration").map(CellValue::Int),
        ),
        (
            mood_field::TODO_ID,
            row.get::<Option<Id>, _>("todo_id")
                .map(|todo| CellValue::text(todo.to_string())),
        ),
    ];
    create(conn, id, "mood", &cells).await
}

/// Emit an edit of one mood field.
pub async fn mood_update(conn: &mut SqliteConnection, id: Id, update: MoodUpdate) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "mood", field, value, None).await?;
    Ok(())
}

/// Emit the creation of a tracker entry.
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
    let score = crate::db::tracker_value(row.get("score_kind"), &row.get::<String, _>("score_text"));
    let cells = [
        (
            tracker_field::TRACKER_TYPE,
            Some(CellValue::text(row.get::<String, _>("type"))),
        ),
        (tracker_field::SCORE, Some(CellValue::from(&score))),
        (
            tracker_field::TIME,
            Some(CellValue::int(row.get::<i64, _>("time"))),
        ),
        (
            tracker_field::MOOD_ID,
            row.get::<Option<Id>, _>("mood")
                .map(|mood| CellValue::text(mood.to_string())),
        ),
    ];
    create(conn, id, "tracker", &cells).await
}

/// Emit an edit of one tracker field.
pub async fn tracker_update(
    conn: &mut SqliteConnection,
    id: Id,
    update: TrackerUpdate,
) -> Result<()> {
    let (field, value) = update.cell();
    append(conn, id, "tracker", field, value, None).await?;
    Ok(())
}

/// Emit a completion row. A delta is an ordinary one-cell entity: its `count`
/// is signed, and totals are the sum of the rows that survive LWW.
pub async fn completion(conn: &mut SqliteConnection, id: Id) -> Result<()> {
    let row = sqlx::query("SELECT todo_id, time, count FROM todo_completions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the completion for its sync event")?;
    let Some(row) = row else { return Ok(()) };
    let cells = [
        (
            completion_field::TODO_ID,
            Some(CellValue::text(
                row.get::<Id, _>("todo_id").to_string(),
            )),
        ),
        (
            completion_field::TIME,
            Some(CellValue::int(row.get::<i64, _>("time"))),
        ),
        (
            completion_field::COUNT,
            Some(CellValue::int(row.get::<i32, _>("count"))),
        ),
    ];
    create(conn, id, "completion", &cells).await
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
        None,
    )
    .await?;
    Ok(())
}

/// Emit a delete. The row is removed by the caller; this keeps a snapshot of
/// what it held, because that is the only place the content still exists once
/// the row is gone.
pub async fn delete(conn: &mut SqliteConnection, id: Id, kind: &str) -> Result<()> {
    let image = row_image(&mut *conn, id, kind).await?;
    append(conn, id, kind, super::types::EXISTENCE, deleted(), image).await?;
    Ok(())
}

/// Append an existence cell and one cell per populated field.
async fn create(
    conn: &mut SqliteConnection,
    id: Id,
    kind: &str,
    cells: &[(&'static str, Option<CellValue>)],
) -> Result<()> {
    append(conn, id, kind, super::types::EXISTENCE, live(), None).await?;
    for (field, value) in cells {
        if let Some(value) = value {
            append(&mut *conn, id, kind, field, Some(value.clone()), None).await?;
        }
    }
    Ok(())
}

/// Append one cell to the outbox, stamped and attributed.
async fn append(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    field: &str,
    value: Option<CellValue>,
    image: Option<String>,
) -> Result<i64> {
    let device = state::device_id(&mut *conn).await?;
    let timestamp = state::next_stamp(&mut *conn).await?;
    let event_id = EventId::new();
    let json = match &value {
        Some(value) => Some(
            serde_json::to_string(value).context("Failed to serialize a cell value")?,
        ),
        None => None,
    };
    sqlx::query(
        "INSERT INTO _sync_events
           (event_id, device_id, entity_id, kind, field, value, timestamp, image)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event_id)
    .bind(device)
    .bind(entity_id)
    .bind(kind)
    .bind(field)
    .bind(json)
    .bind(timestamp)
    .bind(image)
    .execute(&mut *conn)
    .await
    .context("Failed to append a sync event")?;
    Ok(timestamp)
}

/// The cells a row holds right now, as JSON, for a later resurrection.
async fn row_image(conn: &mut SqliteConnection, id: Id, kind: &str) -> Result<Option<String>> {
    let Some(spec) = schema::kind(kind) else {
        return Ok(None);
    };
    let mut selected = Vec::new();
    for (name, field) in &spec.fields {
        let column = field.column.as_deref().unwrap_or(name);
        if field.class == Class::Scalar {
            // A scalar's class is on the value, not the column.
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
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to read the {kind} being deleted"))?;
    let Some(row) = row else { return Ok(None) };

    let mut cells: Vec<(&str, CellValue)> = Vec::new();
    for (name, field) in &spec.fields {
        let column = field.column.as_deref().unwrap_or(name);
        let value = match field.class {
            Class::Text => row
                .get::<Option<String>, _>(column)
                .map(CellValue::Text),
            Class::Int => row.get::<Option<i64>, _>(column).map(CellValue::Int),
            Class::Real => row.get::<Option<f64>, _>(column).map(CellValue::Real),
            Class::Scalar => {
                let kind_key = format!("{column}#kind");
                let text_key = format!("{column}#text");
                let storage: Option<String> = row.get(kind_key.as_str());
                let text: Option<String> = row.get(text_key.as_str());
                match (storage.as_deref(), text) {
                    (Some("integer"), Some(text)) => {
                        text.parse().ok().map(CellValue::Int)
                    }
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
    let image = serde_json::json!({ "kind": kind, "cells": cells });
    Ok(Some(image.to_string()))
}
