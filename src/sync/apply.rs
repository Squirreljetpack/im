//! Applying served cells and verdict repairs.
//!
//! The server already names the winner of every cell, so applying a page is
//! mostly writing values. Two things are deliberate:
//!
//! * a row is **materialised once**, from its existence cell plus the field
//!   cells that arrived with it, in [`KIND_ORDER`] so a referrer's foreign keys
//!   are satisfied;
//! * a cell or verdict repair that cannot be written is skipped and reported in
//!   [`Applied::skipped`], never allowed to poison the rest of the round.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};

use super::schema::{self, KIND_ORDER};
use super::types::{CellEvent, CellValue, DELETED, EXISTENCE, LogRow, completion_field};
use crate::db::{EventId, Id};

/// What one page or verdict pass did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    /// Cells or repairs that could not be written.
    pub skipped: Vec<Skip>,
}

/// One cell that was left alone.
#[derive(Debug, Clone)]
pub struct Skip {
    pub entity_id: Id,
    pub field: String,
    pub reason: String,
}

/// Whether a cell is the marker that ends an entity.
fn is_dead(cell: &CellEvent) -> bool {
    cell.field == EXISTENCE && cell.value.as_ref().and_then(CellValue::as_text) == Some(DELETED)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    /// Served pull row: skip if a newer unpushed local edit in `_sync_events` outranks it.
    Served,
    /// Server verdict repair: overwrite unconditionally with the server's winning cell.
    VerdictRepair,
}

/// Entities whose create arrived without the cells that make a row buildable.
///
/// A create whose required link target has not arrived yet is held here until
/// the target is available, and [`Pending::fence`] says how far the cursor may
/// move while it waits.
#[derive(Debug, Default)]
pub struct Pending {
    held: HashMap<Id, Held>,
}

#[derive(Debug)]
struct Held {
    kind: String,
    /// The existence cell's version: nothing at or below it is in the database.
    since: i64,
    cells: Vec<LogRow>,
    /// Why the last attempt to build the row failed.
    reason: String,
}

impl Pending {
    fn holds(&self, entity: Id) -> bool {
        self.held.contains_key(&entity)
    }

    /// Hold an entity from the existence cell that announced it.
    fn start(&mut self, row: &LogRow) {
        self.held.insert(
            row.cell.entity_id,
            Held {
                kind: row.cell.kind.clone(),
                since: row.version,
                cells: vec![row.clone()],
                reason: String::new(),
            },
        );
    }

    /// Add a cell to a held entity, keeping the newest write of its field.
    fn add(&mut self, row: &LogRow) {
        let Some(held) = self.held.get_mut(&row.cell.entity_id) else {
            return;
        };
        let position = held
            .cells
            .iter()
            .position(|cell| cell.cell.field == row.cell.field);
        match position {
            Some(index) if held.cells[index].version < row.version => {
                held.cells[index] = row.clone();
            }
            Some(_) => {}
            None => held.cells.push(row.clone()),
        }
    }

    /// Drop an entity that was deleted before it could be built: there is
    /// nothing to insert and nothing local to remove.
    fn forget(&mut self, entity: Id) {
        self.held.remove(&entity);
    }

    /// How far the cursor may advance: it may not pass a create whose cells are
    /// not in the database yet.
    pub(crate) fn fence(&self, end: i64) -> i64 {
        self.held
            .values()
            .map(|held| held.since - 1)
            .min()
            .unwrap_or(end)
    }

    /// Nothing more is coming, so what is still held will never be buildable.
    /// Report it rather than pin the cursor behind it for every later pull.
    pub(crate) fn abandon(&mut self) -> Vec<Skip> {
        let mut held: Vec<(Id, Held)> = self.held.drain().collect();
        held.sort_unstable_by_key(|(_, held)| held.since);
        held.into_iter()
            .map(|(entity_id, held)| Skip {
                entity_id,
                field: "create".to_string(),
                reason: if held.reason.is_empty() {
                    "the cells needed to build it never arrived".to_string()
                } else {
                    held.reason
                },
            })
            .collect()
    }

    /// Build every held entity that now has the cells its schema requires.
    async fn flush(
        &mut self,
        conn: &mut SqliteConnection,
        applied: &mut Applied,
        touched_tasks: &mut HashSet<Id>,
    ) -> Result<()> {
        let mut deferred_links: Vec<LogRow> = Vec::new();
        for kind in KIND_ORDER {
            let Some(table) = schema::table(kind) else {
                continue;
            };
            let mut due: Vec<(i64, Id)> = self
                .held
                .iter()
                .filter(|(_, held)| held.kind == kind)
                .map(|(entity, held)| (held.since, *entity))
                .collect();
            due.sort_unstable_by_key(|(since, _)| *since);
            for (_, entity) in due {
                let Some(mut held) = self.held.remove(&entity) else {
                    continue;
                };
                let outcome = {
                    let cells: Vec<&LogRow> = held
                        .cells
                        .iter()
                        .filter(|row| row.cell.field != EXISTENCE)
                        .collect();
                    insert_entity(&mut *conn, kind, table, entity, &cells).await
                };
                match outcome {
                    Ok(omitted) => {
                        applied.created += 1;
                        if held.kind == "task" {
                            touched_tasks.insert(entity);
                        } else if held.kind == "completion" {
                            for row in &held.cells {
                                if row.cell.field == completion_field::TODO_ID
                                    && let Some(text) =
                                        row.cell.value.as_ref().and_then(CellValue::as_text)
                                    && let Ok(task_id) = text.parse::<Id>()
                                {
                                    touched_tasks.insert(task_id);
                                }
                            }
                        }
                        deferred_links.extend(omitted);
                    }
                    Err(error) => {
                        held.reason = format!("{error:#}");
                        self.held.insert(entity, held);
                    }
                }
            }
        }
        // Any optional link omitted during `insert_entity` because its target
        // was built later in the same flush is applied now if the target exists.
        for row in &deferred_links {
            if let Some(link_kind) = schema::link(&row.cell.kind, &row.cell.field)
                && let Some(target) = row.cell.value.as_ref().and_then(CellValue::as_text)
                && let Ok(target_id) = target.parse::<Id>()
                && row_exists(&mut *conn, link_kind, target_id).await?
            {
                apply_cell(&mut *conn, row, applied, ApplyMode::Served).await?;
            }
        }
        Ok(())
    }
}

/// Apply a page of served cells.
///
/// A cell whose entity has no local row yet is held in `pending` until the
/// cells that build the row have arrived, which can take more pages.
pub async fn apply_rows(
    conn: &mut SqliteConnection,
    rows: &[LogRow],
    pending: &mut Pending,
) -> Result<Applied> {
    let mut applied = Applied::default();
    let mut touched_tasks = HashSet::new();
    let mut served: Vec<&LogRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let entity = row.cell.entity_id;
        if pending.holds(entity) {
            if is_dead(&row.cell) {
                pending.forget(entity);
            } else {
                pending.add(row);
            }
            continue;
        }
        if row.cell.field == EXISTENCE && !is_dead(&row.cell) {
            if schema::table(&row.cell.kind).is_none() {
                served.push(row);
                continue;
            }
            if !row_exists(&mut *conn, &row.cell.kind, entity).await? {
                pending.start(row);
                continue;
            }
        }
        served.push(row);
    }

    pending
        .flush(&mut *conn, &mut applied, &mut touched_tasks)
        .await?;
    for row in &served {
        record_touched_task(&mut *conn, &row.cell.kind, row.cell.entity_id, &mut touched_tasks)
            .await?;
        apply_cell(&mut *conn, row, &mut applied, ApplyMode::Served).await?;
    }
    sync_touched_short_ids(&mut *conn, &touched_tasks).await?;
    Ok(applied)
}

/// Record the affected task ID when a `task` or `completion` row is about to be mutated.
async fn record_touched_task(
    conn: &mut SqliteConnection,
    kind: &str,
    entity: Id,
    touched_tasks: &mut HashSet<Id>,
) -> Result<()> {
    if kind == "task" {
        touched_tasks.insert(entity);
    } else if kind == "completion"
        && let Some(task_id) = completion_todo_id(&mut *conn, entity).await?
    {
        touched_tasks.insert(task_id);
    }
    Ok(())
}

/// Recompute `short_id` for every task touched by a page or verdict pass.
async fn sync_touched_short_ids(
    conn: &mut SqliteConnection,
    touched_tasks: &HashSet<Id>,
) -> Result<()> {
    for &task_id in touched_tasks {
        if row_exists(&mut *conn, "task", task_id).await? {
            crate::db::sync_short_id(&mut *conn, task_id).await?;
        }
    }
    Ok(())
}

/// Look up the task ID of an existing completion row before applying an update or delete.
async fn completion_todo_id(conn: &mut SqliteConnection, id: Id) -> Result<Option<Id>> {
    sqlx::query_scalar("SELECT todo_id FROM todo_completions WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read completion todo_id")
}

/// The value a cell materialises as. A null `completion.count` is how an
/// earlier build stored a scheduled task's unmarked state, and the column is
/// NOT NULL, so it lands as the sentinel; every other null cell stays null.
fn stored_value(kind: &str, field: &str, value: &Option<CellValue>) -> Option<CellValue> {
    match value {
        None if kind == "completion" && field == "count" => {
            Some(CellValue::Int(i64::from(crate::db::SCHEDULED_UNMARKED)))
        }
        other => other.clone(),
    }
}

/// Push a bound [`CellValue`] onto a [`QueryBuilder`].
fn push_cell_value(builder: &mut QueryBuilder<Sqlite>, value: Option<CellValue>) {
    match value {
        Some(CellValue::Text(text)) => {
            builder.push_bind(text);
        }
        Some(CellValue::Int(number)) => {
            builder.push_bind(number);
        }
        Some(CellValue::Real(number)) => {
            builder.push_bind(number);
        }
        None => {
            builder.push_bind(None::<String>);
        }
    }
}

/// One `INSERT` from the cells that arrived with the entity's existence cell.
/// The domain tables have `NOT NULL` columns, so this cannot be done one cell
/// at a time. Returns any optional link cells omitted because their target row
/// was not yet present.
async fn insert_entity(
    conn: &mut SqliteConnection,
    kind: &str,
    table: &str,
    entity: Id,
    cells: &[&LogRow],
) -> Result<Vec<LogRow>> {
    let spec = schema::kind(kind).context("unknown kind")?;
    for (name, field) in &spec.fields {
        let populated = cells
            .iter()
            .any(|row| row.cell.field == *name && row.cell.value.is_some());
        anyhow::ensure!(
            !field.required || populated,
            "missing required field {name}"
        );
    }

    let mut included: Vec<&LogRow> = Vec::new();
    let mut deferred: Vec<LogRow> = Vec::new();
    for row in cells {
        if let Some(link_kind) = schema::link(kind, &row.cell.field) {
            let Some(target) = row.cell.value.as_ref().and_then(CellValue::as_text) else {
                continue;
            };
            let Ok(target) = target.parse::<Id>() else {
                continue;
            };
            if !row_exists(&mut *conn, link_kind, target).await? {
                anyhow::ensure!(
                    schema::field(kind, &row.cell.field).is_none_or(|field| !field.required),
                    "missing required field {}: the row it points at has not arrived",
                    row.cell.field
                );
                deferred.push((*row).clone());
                continue;
            }
        }
        included.push(row);
    }

    let mut builder = QueryBuilder::new(format!("INSERT INTO \"{table}\" (id"));
    for row in &included {
        let column = schema::column(kind, &row.cell.field)
            .with_context(|| format!("unknown field {}", row.cell.field))?;
        builder.push(", \"").push(column).push("\"");
    }
    builder.push(") VALUES (").push_bind(entity);
    for row in &included {
        builder.push(", ");
        push_cell_value(
            &mut builder,
            stored_value(kind, &row.cell.field, &row.cell.value),
        );
    }
    builder.push(")");
    builder
        .build()
        .execute(&mut *conn)
        .await
        .context("Failed to insert a served row")?;
    Ok(deferred)
}

/// Write one cell: a value, or the existence cell's `deleted` marker.
async fn apply_cell(
    conn: &mut SqliteConnection,
    row: &LogRow,
    applied: &mut Applied,
    mode: ApplyMode,
) -> Result<()> {
    let kind = row.cell.kind.as_str();
    let entity = row.cell.entity_id;
    let deleting = row.cell.field == EXISTENCE && is_dead(&row.cell);
    let Some(table) = schema::table(kind) else {
        applied.skipped.push(Skip {
            entity_id: entity,
            field: row.cell.field.clone(),
            reason: format!("unknown kind {kind}"),
        });
        return Ok(());
    };
    let outcome = if row.cell.field == EXISTENCE {
        if is_dead(&row.cell) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM \"{table}\" WHERE id = ?"
            )))
            .bind(entity)
            .execute(&mut *conn)
            .await
            .map(|result| {
                if result.rows_affected() > 0 {
                    applied.deleted += 1;
                }
            })
        } else {
            Ok(())
        }
    } else {
        let Some(_spec) = schema::field(kind, &row.cell.field) else {
            applied.skipped.push(Skip {
                entity_id: entity,
                field: row.cell.field.clone(),
                reason: format!("unknown field {}", row.cell.field),
            });
            return Ok(());
        };
        if mode == ApplyMode::Served
            && super::events::pending_outranks(&mut *conn, &row.cell, &HashSet::<EventId>::new())
                .await?
        {
            return Ok(());
        }
        let column = schema::column(kind, &row.cell.field).unwrap_or(&row.cell.field);
        let mut builder = QueryBuilder::new(format!("UPDATE \"{table}\" SET \"{column}\" = "));
        push_cell_value(
            &mut builder,
            stored_value(kind, &row.cell.field, &row.cell.value),
        );
        builder.push(" WHERE id = ").push_bind(entity);
        builder
            .build()
            .execute(&mut *conn)
            .await
            .map(|result| {
                if result.rows_affected() > 0 {
                    applied.updated += 1;
                } else {
                    applied.skipped.push(Skip {
                        entity_id: entity,
                        field: row.cell.field.clone(),
                        reason: "no local row".to_string(),
                    });
                }
            })
    };
    match outcome {
        Err(error) => applied.skipped.push(Skip {
            entity_id: entity,
            field: row.cell.field.clone(),
            reason: error.to_string(),
        }),
        Ok(()) => {
            if deleting {
                super::events::retire_cascade(&mut *conn, kind, entity).await?;
            }
        }
    }
    Ok(())
}

/// The `(type, stamp)` a tracker row currently holds, for slot bookkeeping.
pub async fn tracker_slot_key(
    conn: &mut SqliteConnection,
    id: Id,
) -> Result<Option<(String, crate::date::LogTime)>> {
    let row = sqlx::query("SELECT type, time, tz_offset FROM tracker WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read a tracker's slot")?;
    Ok(row.map(|row| (row.get("type"), crate::db::log_time(&row))))
}

/// Collapse replace-mode tracker rows that landed in one slot.
///
/// The slot is `(type, time window)` on the configured grid, so it is not a
/// link and the server cannot check it. The winner is the highest `(time, id)`
/// rank — both parts are on the row itself and fixed at birth, so any two
/// devices holding the same rows rank them the same way. The losers are deleted
/// *with* an event: a delete is terminal, so an unpublished one would leave
/// them alive on every peer.
pub async fn dedup_tracker_slots(
    conn: &mut SqliteConnection,
    touched: &HashSet<(String, (i64, i64))>,
) -> Result<usize> {
    let mut removed = 0;
    for (tracker_type, (low, high)) in touched {
        let candidates = sqlx::query(
            "SELECT id, time, tz_offset FROM tracker
              WHERE type = ? AND time >= ? AND time < ?
              ORDER BY time DESC, id DESC",
        )
        .bind(tracker_type)
        .bind(low - crate::db::CIVIL_FETCH_MARGIN)
        .bind(high + crate::db::CIVIL_FETCH_MARGIN)
        .fetch_all(&mut *conn)
        .await
        .context("Failed to read a tracker slot")?;
        let rows: Vec<(Id, i64)> = candidates
            .iter()
            .filter(|row| {
                let rebased = crate::db::log_time(row).rebased();
                rebased >= *low && rebased < *high
            })
            .map(|row| (row.get("id"), row.get("time")))
            .collect();
        for (id, _) in rows.into_iter().skip(1) {
            let deleted = sqlx::query("DELETE FROM tracker WHERE id = ?")
                .bind(id)
                .execute(&mut *conn)
                .await
                .context("Failed to drop a tracker slot loser")?;
            if deleted.rows_affected() > 0 {
                super::events::delete(&mut *conn, id, "tracker").await?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Whether a local row exists for `(kind, id)`.
pub async fn row_exists(conn: &mut SqliteConnection, kind: &str, id: Id) -> Result<bool> {
    let Some(table) = schema::table(kind) else {
        return Ok(false);
    };
    let found: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM \"{table}\" WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .with_context(|| format!("Failed to look up a row in {table}"))?;
    Ok(found.is_some())
}

/// Apply a single cell during verdict reconciliation.
pub async fn apply_one(conn: &mut SqliteConnection, row: &LogRow) -> Result<()> {
    let mut touched_tasks = HashSet::new();
    record_touched_task(
        &mut *conn,
        &row.cell.kind,
        row.cell.entity_id,
        &mut touched_tasks,
    )
    .await?;
    let mut applied = Applied::default();
    apply_cell(&mut *conn, row, &mut applied, ApplyMode::VerdictRepair).await?;
    sync_touched_short_ids(&mut *conn, &touched_tasks).await
}

/// Reset a single field on `(kind, entity)` to its default (`NULL`, or the
/// unmarked sentinel for `completion.count`) when a rejected write has no
/// server winner cell.
pub async fn clear_field(
    conn: &mut SqliteConnection,
    kind: &str,
    entity: Id,
    field: &str,
) -> Result<()> {
    let (Some(table), Some(column)) = (schema::table(kind), schema::column(kind, field)) else {
        return Ok(());
    };
    let mut touched_tasks = HashSet::new();
    record_touched_task(&mut *conn, kind, entity, &mut touched_tasks).await?;
    let mut builder = QueryBuilder::new(format!("UPDATE \"{table}\" SET \"{column}\" = "));
    push_cell_value(&mut builder, stored_value(kind, field, &None));
    builder.push(" WHERE id = ").push_bind(entity);
    builder
        .build()
        .execute(&mut *conn)
        .await
        .with_context(|| format!("Failed to clear {table}.{column}"))?;
    sync_touched_short_ids(&mut *conn, &touched_tasks).await
}

/// Drop an entity's local row without a served death (a refused create).
pub async fn forget(conn: &mut SqliteConnection, kind: &str, entity: Id) -> Result<()> {
    let Some(table) = schema::table(kind) else {
        return Ok(());
    };
    let mut touched_tasks = HashSet::new();
    record_touched_task(&mut *conn, kind, entity, &mut touched_tasks).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM \"{table}\" WHERE id = ?"
    )))
    .bind(entity)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("Failed to remove a refused row from {table}"))?;
    sync_touched_short_ids(&mut *conn, &touched_tasks).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_field_has_a_column() {
        for kind in KIND_ORDER {
            let spec = schema::kind(kind).expect("kind");
            for (name, field) in &spec.fields {
                assert!(
                    schema::column(kind, name).is_some(),
                    "{kind}.{name} has no column"
                );
                if field.class == schema::Class::Scalar {
                    assert_eq!(schema::link(kind, name), None);
                }
            }
        }
    }
}
