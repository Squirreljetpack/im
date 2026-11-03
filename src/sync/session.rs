//! One push+pull round trip.
//!
//! All database changes for a sync round execute inside a single SQLite
//! transaction once the HTTP exchange returns: pre-delete resurrection
//! snapshots, pulled cell application, cursor advancement, tracker slot
//! deduplication, verdict reconciliation, and outbox retirement commit
//! atomically.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{Connection, Row, SqliteConnection, SqlitePool};

use super::apply::{self, Pending, Skip};
use super::client::{Client, run_blocking};
use super::events;
use super::schema;
use super::state;
use super::types::{
    self, CellEvent, CellValue, Cursor, EXISTENCE, LogRow, Notice, Outcome, SyncRequest,
    SyncResponse, Verdict,
};
use crate::db::{EventId, Id};
use crate::tracker::TrackerSlots;

pub use super::client::{account_status, login, register};
pub use super::events::unsynced_count;
pub use super::state::{
    device_id as device_id_of, logout, pool_cursor as cursor, signed_in, store_account, store_token,
};

/// What one round trip did.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Cells handed to the server.
    pub pushed: usize,
    /// Local rows materialised from the page.
    pub created: usize,
    /// Local cells written from the page.
    pub applied: usize,
    /// Local rows removed by a served delete.
    pub deleted: usize,
    /// Cells of the page that could not be written.
    pub skipped: Vec<Skip>,
    /// One line per decision the user should know about.
    pub notices: Vec<String>,
    /// Content of tasks and moods deleted elsewhere, offered as a copy.
    pub resurrect: Vec<Resurrect>,
    /// The cursor this round started from, so the caller can prove progress
    /// when the server says another page is waiting.
    pub cursor_before: i64,
    /// The version the next page is requested from.
    pub cursor: i64,
    pub has_more: bool,
    /// Cells still queued locally (tracker slot repairs queue their own).
    pub pending: usize,
    /// Set when the server refused stamps as too far ahead: `server_now`.
    pub clock_ahead: Option<i64>,
}

/// A deletion this device can offer to re-create.
#[derive(Debug, Clone)]
pub struct Resurrect {
    pub entity_id: Id,
    pub kind: String,
    /// The JSON snapshot of the local row before the remote delete removed it.
    pub image: String,
    pub device: Option<String>,
    pub at: Option<i64>,
}

/// A pull in progress: where it has reached, and the entities it is building.
///
/// `cursor` can run ahead of the low-water mark persisted in `_sync_state`,
/// because a page is applied even while a create in it is still being built.
/// The persisted mark, not this, is what a later run trusts: it never claims a
/// cell that lives only in memory.
pub struct Pull {
    /// The log and version the next page is requested from; `None` until a
    /// sync names one.
    cursor: Option<Cursor>,
    pending: Pending,
}

impl Pull {
    /// Start where the last pull stopped.
    pub async fn resume(pool: &SqlitePool) -> Result<Pull> {
        let mut conn = pool
            .acquire()
            .await
            .context("Failed to acquire a db connection")?;
        Ok(Pull {
            cursor: state::cursor(&mut conn).await?,
            pending: Pending::default(),
        })
    }
}

/// Push the outbox and apply one pull.
pub async fn sync_once(
    pool: &SqlitePool,
    server: &str,
    timeout: Duration,
    slots: &TrackerSlots,
    pull: &mut Pull,
) -> Result<SyncReport> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    let token = state::token(&mut conn)
        .await?
        .context("not logged in — run `im :sync login`")?;
    let device = state::device_id(&mut conn).await?;

    let batch = events::outbox(&mut conn).await?;
    let request = SyncRequest {
        protocol: schema::SCHEMA.protocol,
        schema: schema::SCHEMA.version,
        device_id: device,
        cursor: pull.cursor.clone(),
        events: batch.clone(),
    };
    let client = Client::with_timeout(server, Some(token), timeout);
    let response: SyncResponse = run_blocking(move || client.sync(&request)).await?;

    // One verdict per pushed event is the protocol's contract: without it this
    // device cannot tell which of its own writes were accepted, and retiring
    // the batch would lose the record of the rest. Keep the outbox and report.
    anyhow::ensure!(
        response.verdicts.len() == batch.len(),
        "the server returned {} verdict(s) for {} pushed event(s); keeping the outbox",
        response.verdicts.len(),
        batch.len()
    );

    let mut report = SyncReport {
        pushed: batch.len(),
        cursor_before: pull.cursor.as_ref().map_or(0, |cursor| cursor.version),
        cursor: response.cursor.version,
        has_more: response.has_more,
        ..SyncReport::default()
    };

    let mut tx = conn.begin().await.context("Failed to start a sync round")?;

    // Snapshot any local task or mood whose edit was rejected because a peer
    // deleted the entity, before `apply_rows` or `reconcile` removes the local row.
    let snapshots = snapshot_deleted_edits(&mut tx, &response.verdicts, &batch).await?;

    let applied = apply::apply_rows(&mut tx, &response.events, &mut pull.pending).await?;
    let mut fence = response.cursor.clone();
    if response.has_more {
        fence.version = pull.pending.fence(response.cursor.version);
    }
    state::set_cursor(&mut tx, Some(&fence)).await?;
    report.created = applied.created;
    report.applied = applied.updated;
    report.deleted = applied.deleted;
    report.skipped = applied.skipped;
    if !response.has_more {
        report.skipped.extend(pull.pending.abandon());
    }

    let touched = tracker_slots(&mut tx, slots, &response.events).await?;
    let repaired = apply::dedup_tracker_slots(&mut tx, &touched).await?;
    if repaired > 0 {
        report.notices.push(format!(
            "Dropped {repaired} tracker entr{} that lost its slot.",
            if repaired == 1 { "y" } else { "ies" }
        ));
    }

    reconcile(
        &mut tx,
        &response.verdicts,
        &batch,
        response.now,
        snapshots,
        &mut report,
    )
    .await?;
    explain_lost(&mut tx, &response.superseded, &batch, &mut report).await?;

    events::retire(&mut tx, &batch).await?;
    report.pending = events::unsynced_in(&mut tx).await?;

    tx.commit().await.context("Failed to commit a sync round")?;
    pull.cursor = Some(response.cursor);
    Ok(report)
}

/// Capture pre-delete JSON snapshots for any local `task` or `mood` whose pushed
/// edit was rejected with a deletion tombstone.
async fn snapshot_deleted_edits(
    conn: &mut SqliteConnection,
    verdicts: &[Verdict],
    batch: &[CellEvent],
) -> Result<HashMap<Id, String>> {
    let mut snapshots = HashMap::new();
    for verdict in verdicts {
        if verdict.outcome != Outcome::Rejected
            || verdict.reason.as_deref() != Some("deleted")
            || verdict.tombstone.is_none()
        {
            continue;
        }
        let Some(event) = batch.iter().find(|event| event.event_id == verdict.event_id) else {
            continue;
        };
        if !matches!(event.kind.as_str(), "task" | "mood")
            || snapshots.contains_key(&event.entity_id)
        {
            continue;
        }
        if let Some(image) = events::row_image(&mut *conn, event.entity_id, &event.kind).await? {
            snapshots.insert(event.entity_id, image);
        }
    }
    Ok(snapshots)
}

/// Turn supersession notices into prose. The winning value arrived in the same
/// response and has already been applied, so this only says why a value the
/// user was looking at changed underneath them.
async fn explain_lost(
    conn: &mut SqliteConnection,
    notices: &[Notice],
    batch: &[CellEvent],
    report: &mut SyncReport,
) -> Result<()> {
    let pushed: HashSet<EventId> = batch.iter().map(|event| event.event_id).collect();
    for notice in notices {
        if !apply::row_exists(&mut *conn, &notice.winner.cell.kind, notice.entity_id).await? {
            continue;
        }
        if unpushed_outranks(&mut *conn, &notice.winner, &pushed).await? {
            continue;
        }
        report.notices.push(format!(
            "{} {}: yours ({}) was replaced by {} ({} at {})",
            short(notice.entity_id),
            notice.field,
            describe_value(&notice.lost),
            describe_value(&notice.winner.cell.value),
            notice.winner.cell.device_id,
            format_stamp(notice.winner.cell.timestamp),
        ));
    }
    Ok(())
}

/// The tracker slots this page touched, so slot collisions can be settled once.
async fn tracker_slots(
    conn: &mut SqliteConnection,
    slots: &TrackerSlots,
    rows: &[LogRow],
) -> Result<HashSet<(String, (i64, i64))>> {
    let tracker_ids: HashSet<Id> = rows
        .iter()
        .filter(|row| row.cell.kind == "tracker" && row.cell.field != EXISTENCE)
        .map(|row| row.cell.entity_id)
        .collect();
    let mut touched = HashSet::new();
    for entity_id in tracker_ids {
        let Some((tracker_type, time)) = apply::tracker_slot_key(&mut *conn, entity_id).await?
        else {
            continue;
        };
        if let Some(window) = slots.slot(&tracker_type, time) {
            touched.insert((tracker_type, window));
        }
    }
    Ok(touched)
}

/// Turn verdicts into local state: repair what lost, retire what was refused.
async fn reconcile(
    conn: &mut SqliteConnection,
    verdicts: &[Verdict],
    batch: &[CellEvent],
    now: i64,
    mut snapshots: HashMap<Id, String>,
    report: &mut SyncReport,
) -> Result<()> {
    let pushed: HashSet<EventId> = batch.iter().map(|event| event.event_id).collect();
    let mut ahead_events: Vec<CellEvent> = Vec::new();
    let mut deleted_entities: HashSet<Id> = HashSet::new();

    for verdict in verdicts {
        match verdict.outcome {
            Outcome::Accepted => {}
            Outcome::Superseded => {
                let Some(winner) = &verdict.winner else {
                    continue;
                };
                if unpushed_outranks(&mut *conn, winner, &pushed).await? {
                    continue;
                }
                apply::apply_one(&mut *conn, winner).await?;
                report.notices.push(format!(
                    "{} {} reverted to {} ({} at {})",
                    verdict.event_id,
                    describe(winner),
                    describe_value(&winner.cell.value),
                    winner.cell.device_id,
                    format_stamp(winner.cell.timestamp),
                ));
            }
            Outcome::Rejected => {
                let reason = verdict.reason.as_deref().unwrap_or("unknown");
                if reason == "clock_ahead" {
                    report.clock_ahead = Some(now);
                    if let Some(event) = batch.iter().find(|e| e.event_id == verdict.event_id) {
                        ahead_events.push(event.clone());
                    }
                    continue;
                }
                if let Some(tombstone) = &verdict.tombstone {
                    apply::apply_one(&mut *conn, tombstone).await?;
                }
                let Some(event) = batch.iter().find(|event| event.event_id == verdict.event_id)
                else {
                    continue;
                };

                if reason == "deleted" && verdict.tombstone.is_some() {
                    if deleted_entities.insert(event.entity_id) {
                        let device = verdict
                            .tombstone
                            .as_ref()
                            .map(|cell| cell.cell.device_id.to_string())
                            .unwrap_or_else(|| "another device".to_string());
                        let at = verdict
                            .tombstone
                            .as_ref()
                            .map(|cell| format_stamp(cell.cell.timestamp))
                            .unwrap_or_default();
                        report.notices.push(format!(
                            "{} was deleted on {device} at {at}; the local edit was dropped",
                            short(event.entity_id),
                        ));
                        if let Some(image) = snapshots.remove(&event.entity_id) {
                            report.resurrect.push(Resurrect {
                                entity_id: event.entity_id,
                                kind: event.kind.clone(),
                                image,
                                device: verdict
                                    .tombstone
                                    .as_ref()
                                    .map(|cell| cell.cell.device_id.to_string()),
                                at: verdict
                                    .tombstone
                                    .as_ref()
                                    .map(|cell| cell.cell.timestamp),
                            });
                        }
                    }
                } else if let Some(winner) = &verdict.winner {
                    apply::apply_one(&mut *conn, winner).await?;
                    report.notices.push(format!(
                        "{} refused ({reason}); showing {} instead",
                        describe(winner),
                        describe_value(&winner.cell.value),
                    ));
                } else if event.field == EXISTENCE
                    || schema::field(&event.kind, &event.field).is_some_and(|field| field.required)
                {
                    // A refused create (or a refused required field on a create)
                    // leaves no local row behind.
                    apply::forget(&mut *conn, &event.kind, event.entity_id).await?;
                    report
                        .notices
                        .push(format!("refused ({reason}): {}", short(event.entity_id)));
                } else {
                    apply::clear_field(&mut *conn, &event.kind, event.entity_id, &event.field)
                        .await?;
                    report.notices.push(format!(
                        "{}.{} refused ({reason}); showing empty instead",
                        short(event.entity_id),
                        event.field,
                    ));
                }
            }
        }
    }

    if !ahead_events.is_empty() {
        events::restamp(&mut *conn, &ahead_events, now).await?;
    }
    Ok(())
}

/// Whether a local edit that was *not* part of this push outranks a served cell.
async fn unpushed_outranks(
    conn: &mut SqliteConnection,
    winner: &LogRow,
    pushed: &HashSet<EventId>,
) -> Result<bool> {
    let rows = sqlx::query(
        "SELECT event_id, timestamp FROM _sync_events WHERE entity_id = ? AND field = ?",
    )
    .bind(winner.cell.entity_id)
    .bind(&winner.cell.field)
    .fetch_all(&mut *conn)
    .await
    .context("Failed to read the outbox for a cell")?;
    Ok(rows.iter().any(|row| {
        let event_id: EventId = row.get("event_id");
        let timestamp: i64 = row.get("timestamp");
        !pushed.contains(&event_id) && timestamp > winner.cell.timestamp
    }))
}

/// A stamp (milliseconds) as the user reads it.
fn format_stamp(stamp: i64) -> String {
    crate::date::format_human_datetime(stamp / 1000, false)
}

fn describe(row: &LogRow) -> String {
    format!(
        "{}.{}",
        short(row.cell.entity_id),
        if row.cell.field == EXISTENCE {
            "existence"
        } else {
            &row.cell.field
        }
    )
}

fn describe_value(value: &Option<CellValue>) -> String {
    match value {
        Some(CellValue::Text(text)) => format!("\"{text}\""),
        Some(CellValue::Int(number)) => number.to_string(),
        Some(CellValue::Real(number)) => number.to_string(),
        None => "empty".to_string(),
    }
}

fn short(id: Id) -> String {
    let text = id.to_string();
    text.chars().take(8).collect()
}

/// Re-create a deleted entity from the snapshot captured before the remote
/// delete removed it.
///
/// This decides nothing about which write wins: the copy is a new entity with a
/// fresh id, published as an ordinary create, so peers accept it without ever
/// seeing a question. A link that no longer resolves is dropped rather than
/// re-pointed at a guess.
pub async fn resurrect(pool: &SqlitePool, offer: &Resurrect) -> Result<Option<Id>> {
    let Ok(image) = serde_json::from_str::<serde_json::Value>(&offer.image) else {
        return Ok(None);
    };
    let Some(cells) = image.get("cells").and_then(|value| value.as_array()) else {
        return Ok(None);
    };
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    let device = state::device_id(&mut conn).await?;
    let stamp = state::next_stamp(&mut conn).await?;
    let entity = Id::new();
    let mut rows = vec![LogRow {
        version: 0,
        cell: CellEvent {
            event_id: EventId::new(),
            device_id: device,
            entity_id: entity,
            kind: offer.kind.clone(),
            field: EXISTENCE.to_string(),
            value: types::live(),
            timestamp: stamp,
        },
    }];
    for cell in cells {
        let Some(pair) = cell.as_array() else { continue };
        let Some(field) = pair.first().and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(value) = pair.get(1) else { continue };
        let value: Option<CellValue> = serde_json::from_value(value.clone()).ok();
        if let Some(link) = schema::link(&offer.kind, field) {
            let Some(target) = value.as_ref().and_then(CellValue::as_text) else {
                continue;
            };
            let Some(target) = target.parse::<Id>().ok() else { continue };
            if !apply::row_exists(&mut conn, link, target).await? {
                continue;
            }
        }
        rows.push(LogRow {
            version: 0,
            cell: CellEvent {
                event_id: EventId::new(),
                device_id: device,
                entity_id: entity,
                kind: offer.kind.clone(),
                field: field.to_string(),
                value,
                timestamp: stamp,
            },
        });
    }
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start the resurrection")?;
    let applied = apply::apply_rows(&mut tx, &rows, &mut Pending::default()).await?;
    if applied.created == 0 {
        return Ok(None);
    }
    match offer.kind.as_str() {
        "task" => events::task_create(&mut tx, entity).await?,
        "mood" => events::mood_create(&mut tx, entity).await?,
        _ => {}
    }
    tx.commit()
        .await
        .context("Failed to commit the resurrection")?;
    Ok(Some(entity))
}

/// Throw the local copy away: the next pull rebuilds it from the register.
pub async fn reset_local_state(pool: &SqlitePool) -> Result<usize> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    let mut tx = conn
        .begin()
        .await
        .context("Failed to start the reset transaction")?;
    let discarded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sync_events")
        .fetch_one(&mut *tx)
        .await
        .context("Failed to count the queued events")?;
    for table in [
        "todo_completions",
        "tracker",
        "mood",
        "todos",
        "_sync_events",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to clear {table}"))?;
    }
    // The account lives in its own table, so this is the whole reset: the
    // device id rotates and the cursor and clock are forgotten, while the
    // sign-in survives.
    sqlx::query("DELETE FROM _sync_state")
        .execute(&mut *tx)
        .await
        .context("Failed to clear the sync state")?;
    tx.commit()
        .await
        .context("Failed to commit the reset transaction")?;
    Ok(discarded as usize)
}
