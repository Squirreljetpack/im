//! Applying served cells.
//!
//! The server already names the winner of every cell, so applying a page is
//! mostly writing values. Two things are deliberate:
//!
//! * a row is **materialised once**, from its existence cell plus the field
//!   cells that arrived with it, in [`KIND_ORDER`] so a referrer's foreign keys
//!   are satisfied;
//! * a cell that cannot be written is skipped and reported, never allowed to
//!   poison the rest of the page.

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection};

use super::schema::{self, KIND_ORDER};
use super::types::{CellEvent, CellValue, DELETED, EXISTENCE, LogRow};
use crate::db::Id;

/// What one page did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    /// Cells that could not be written.
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

/// Entities whose create arrived without the cells that make a row buildable.
///
/// A pull is cut by register version, and a required cell's register row moves
/// forward whenever that cell is rewritten — a rename, a rescore. So the
/// existence cell can arrive on one page and its required cells on another, and
/// the domain tables cannot be filled one cell at a time: the row is one
/// `INSERT`, so every cell it needs has to be in hand. Those cells are held
/// here until they are, and [`Pending::fence`] says how far the cursor may move
/// while they wait — which is what lets them stay in memory.
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
    async fn flush(&mut self, conn: &mut SqliteConnection, applied: &mut Applied) -> Result<()> {
        // Insert first, in `KIND_ORDER` so a referrer's target is built before
        // it; a row still short a required cell stays held.
        let mut built: Vec<(Id, Held)> = Vec::new();
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
                    Ok(()) => {
                        applied.created += 1;
                        built.push((entity, held));
                    }
                    Err(error) => {
                        held.reason = format!("{error:#}");
                        self.held.insert(entity, held);
                    }
                }
            }
        }
        // The row, then its cells: a link the insert had to leave out, because
        // its target was built later in this same flush, is written now that
        // every row exists.
        for (entity, held) in built {
            for row in &held.cells {
                if row.cell.field != EXISTENCE {
                    apply_cell(&mut *conn, row, applied).await?;
                }
            }
            if held.kind == "task" {
                crate::db::sync_short_id(&mut *conn, entity).await?;
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
            let Some(table) = schema::table(&row.cell.kind) else {
                served.push(row);
                continue;
            };
            if !exists(&mut *conn, table, entity).await? {
                pending.start(row);
                continue;
            }
        }
        served.push(row);
    }

    pending.flush(&mut *conn, &mut applied).await?;
    for row in &served {
        apply_cell(&mut *conn, row, &mut applied).await?;
    }
    // A task's short id is a local projection: a task that just appeared needs
    // one, and a completion that landed can retire it.
    for row in &served {
        if row.cell.kind == "task" {
            crate::db::sync_short_id(&mut *conn, row.cell.entity_id).await?;
        }
    }
    Ok(applied)
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

/// One `INSERT` from the cells that arrived with the entity's existence cell.
/// The domain tables have `NOT NULL` columns, so this cannot be done one cell
/// at a time.
async fn insert_entity(
    conn: &mut SqliteConnection,
    kind: &str,
    table: &str,
    entity: Id,
    cells: &[&LogRow],
) -> Result<()> {
    let spec = schema::kind(kind).context("unknown kind")?;
    for (name, field) in &spec.fields {
        let populated = cells
            .iter()
            .any(|row| row.cell.field == *name && row.cell.value.is_some());
        anyhow::ensure!(!field.required || populated, "missing required field {name}");
    }

    // A link is written with the row only when its referent is already here;
    // otherwise the second pass applies it once every row exists. Completions
    // come last in `KIND_ORDER`, so their `NOT NULL` task link is always
    // available.
    let mut included: Vec<&LogRow> = Vec::new();
    for row in cells {
        if let Some(link_kind) = schema::link(kind, &row.cell.field) {
            let Some(target) = row.cell.value.as_ref().and_then(CellValue::as_text) else {
                continue;
            };
            let Ok(target) = target.parse::<Id>() else {
                continue;
            };
            let Some(link_table) = schema::table(link_kind) else {
                continue;
            };
            if !exists(&mut *conn, link_table, target).await? {
                // Leaving out a required link is not an option: the row would
                // stay unbuildable until the target arrives.
                anyhow::ensure!(
                    schema::field(kind, &row.cell.field).is_none_or(|field| !field.required),
                    "missing required field {}: the row it points at has not arrived",
                    row.cell.field
                );
                continue;
            }
        }
        included.push(row);
    }

    let mut builder = sqlx::QueryBuilder::new(format!("INSERT INTO \"{table}\" (id"));
    for row in &included {
        let column = schema::column(kind, &row.cell.field)
            .with_context(|| format!("unknown field {}", row.cell.field))?;
        builder.push(", \"").push(column).push("\"");
    }
    builder.push(") VALUES (").push_bind(entity);
    for row in &included {
        builder.push(", ");
        match &stored_value(kind, &row.cell.field, &row.cell.value) {
            Some(CellValue::Text(text)) => {
                builder.push_bind(text.clone());
            }
            Some(CellValue::Int(number)) => {
                builder.push_bind(*number);
            }
            Some(CellValue::Real(number)) => {
                builder.push_bind(*number);
            }
            None => {
                builder.push_bind(None::<String>);
            }
        }
    }
    builder.push(")");
    builder
        .build()
        .execute(&mut *conn)
        .await
        .context("Failed to insert a served row")?;
    Ok(())
}

/// Write one cell: a value, or the existence cell's `deleted` marker.
async fn apply_cell(
    conn: &mut SqliteConnection,
    row: &LogRow,
    applied: &mut Applied,
) -> Result<()> {
    let kind = row.cell.kind.as_str();
    let entity = row.cell.entity_id;
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
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM \"{table}\" WHERE id = ?")))
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
        let Some(spec) = schema::field(kind, &row.cell.field) else {
            applied.skipped.push(Skip {
                entity_id: entity,
                field: row.cell.field.clone(),
                reason: format!("unknown field {}", row.cell.field),
            });
            return Ok(());
        };
        let column = schema::column(kind, &row.cell.field).unwrap_or(&row.cell.field);
        // A scalar (tracker score) keeps the storage class the cell carries;
        // everything else has one class per column.
        let _ = spec.class;
        let sql = format!("UPDATE \"{table}\" SET \"{column}\" = ? WHERE id = ?");
        let value = stored_value(kind, &row.cell.field, &row.cell.value);
        let result = match &value {
            Some(CellValue::Text(text)) => {
                sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(text).bind(entity).execute(&mut *conn).await
            }
            Some(CellValue::Int(number)) => {
                sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(number).bind(entity).execute(&mut *conn).await
            }
            Some(CellValue::Real(number)) => {
                sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(number).bind(entity).execute(&mut *conn).await
            }
            None => {
                sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                    .bind(None::<String>)
                    .bind(entity)
                    .execute(&mut *conn)
                    .await
            }
        };
        result.and_then(|result| {
            if result.rows_affected() > 0 {
                applied.updated += 1;
                Ok(())
            } else {
                applied.skipped.push(Skip {
                    entity_id: entity,
                    field: row.cell.field.clone(),
                    reason: "no local row".to_string(),
                });
                Ok(())
            }
        })
    };
    if let Err(error) = outcome {
        applied.skipped.push(Skip {
            entity_id: entity,
            field: row.cell.field.clone(),
            reason: error.to_string(),
        });
    }
    Ok(())
}

/// The `(type, time)` a tracker row currently holds, for slot bookkeeping.
pub async fn tracker_slot_key(
    conn: &mut SqliteConnection,
    id: Id,
) -> Result<Option<(String, i64)>> {
    let row = sqlx::query("SELECT type, time FROM tracker WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read a tracker's slot")?;
    Ok(row.map(|row| (row.get("type"), row.get("time"))))
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
    touched: &std::collections::HashSet<(String, (i64, i64))>,
) -> Result<usize> {
    let mut removed = 0;
    for (tracker_type, (low, high)) in touched {
        let rows: Vec<(Id, i64)> = sqlx::query_as(
            "SELECT id, time FROM tracker
              WHERE type = ? AND time >= ? AND time < ?
              ORDER BY time DESC, id DESC",
        )
        .bind(tracker_type)
        .bind(low)
        .bind(high)
        .fetch_all(&mut *conn)
        .await
        .context("Failed to read a tracker slot")?;
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

/// Whether a row exists: the only question `materialise` asks.
async fn exists(conn: &mut SqliteConnection, table: &str, id: Id) -> Result<bool> {
    let found: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM \"{table}\" WHERE id = ?"
    )))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to look up a row in {table}"))?;
    Ok(found.is_some())
}

/// Apply a single cell, for verdict repair.
pub async fn apply_one(conn: &mut SqliteConnection, row: &LogRow) -> Result<()> {
    let mut applied = Applied::default();
    apply_cell(conn, row, &mut applied).await
}

/// Drop an entity's local row without a served death (a refused create).
pub async fn forget(conn: &mut SqliteConnection, kind: &str, entity: Id) -> Result<()> {
    let Some(table) = schema::table(kind) else {
        return Ok(());
    };
    sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM \"{table}\" WHERE id = ?")))
        .bind(entity)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("Failed to drop a refused {kind}"))?;
    Ok(())
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
