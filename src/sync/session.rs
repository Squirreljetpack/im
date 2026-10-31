//! One push+pull round trip.
//!
//! The order is load-bearing: apply the served cells, then read the verdicts
//! (which may repair a cell from the winner they carry), then retire the batch
//! that was actually sent. Retiring first would destroy the only record of what
//! this device tried to write.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{Connection, Row, SqliteConnection, SqlitePool};

use super::apply::{self, Pending, Skip};
use super::client::{Account, AccountStatus, Client, run_blocking};
use super::schema;
use super::state;
use super::types::{
    self, CellEvent, CellValue, Cursor, EXISTENCE, LogRow, Notice, Outcome, SyncRequest,
    SyncResponse, Verdict,
};
use crate::db::{EventId, Id};
use crate::tracker::TrackerSlots;

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
    /// The JSON snapshot the delete event carried.
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

/// Store the account a successful `im :sync register`/`login` returned.
pub async fn store_account(pool: &SqlitePool, account: &Account) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    let previous = state::account_user_id(&mut conn).await?;
    if previous.as_deref() != Some(account.user_id.as_str()) {
        // A different account's log is not this database's position.
        state::set_cursor(&mut conn, None).await?;
    }
    state::set_account(&mut conn, Some(&account.user_id), &account.token).await
}

/// Sign a database in with a token someone else obtained (`im :sync login` on a
/// second machine hands the token over the wire, not the user id).
pub async fn store_token(pool: &SqlitePool, token: &str) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::set_account(&mut conn, None, token).await
}

/// The signed-in account's token, if any.
pub async fn signed_in(pool: &SqlitePool) -> Result<Option<String>> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::token(&mut conn).await
}

/// The log this database has read to, and how far, if it has synced anywhere.
pub async fn cursor(pool: &SqlitePool) -> Result<Option<Cursor>> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::cursor(&mut conn).await
}

/// Forget the account. Local data and the device identity are untouched.
pub async fn logout(pool: &SqlitePool) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::clear_account(&mut conn).await
}

/// The connection-level device id (for callers that already hold one).
pub async fn device_id_of(conn: &mut SqliteConnection) -> Result<Id> {
    state::device_id(conn).await
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

    let batch = outbox(&mut conn).await?;
    let request = SyncRequest {
        protocol: schema::SCHEMA.protocol,
        schema: schema::SCHEMA.version,
        device_id: device,
        cursor: pull.cursor.clone(),
        events: batch.iter().map(|row| row.1.clone()).collect(),
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

    // The page and the cursor commit together: a crash replays the page rather
    // than skipping it. The cursor is a fence and may only pass cells that are
    // in the database — a create still being built is not, so it holds the fence
    // back and the next run is re-served the cells above it. That is what lets a
    // held create stay in memory.
    let mut tx = conn.begin().await.context("Failed to start a sync round")?;
    let applied = apply::apply_rows(&mut tx, &response.events, &mut pull.pending).await?;
    let mut fence = response.cursor.clone();
    if response.has_more {
        fence.version = pull.pending.fence(response.cursor.version);
    }
    state::set_cursor(&mut tx, Some(&fence)).await?;
    tx.commit().await.context("Failed to commit a sync round")?;
    report.created = applied.created;
    report.applied = applied.updated;
    report.deleted = applied.deleted;
    report.skipped = applied.skipped;
    if !response.has_more {
        // Nothing else is coming, so a held create can never be completed.
        // Reporting it beats a fence that would never move again.
        report.skipped.extend(pull.pending.abandon());
    }
    pull.cursor = Some(response.cursor);

    let touched = tracker_slots(&mut conn, slots, &response.events).await?;
    let repaired = apply::dedup_tracker_slots(&mut conn, &touched).await?;
    if repaired > 0 {
        report.notices.push(format!(
            "Dropped {repaired} tracker entr{} that lost its slot.",
            if repaired == 1 { "y" } else { "ies" }
        ));
    }

    reconcile(&mut conn, &response.verdicts, &batch, response.now, &mut report).await?;
    explain_lost(&mut conn, &response.superseded, &batch, &mut report).await?;

    for (event_id, _) in &batch {
        sqlx::query("DELETE FROM _sync_events WHERE event_id = ?")
            .bind(event_id)
            .execute(&mut *conn)
            .await
            .context("Failed to retire a pushed event")?;
    }
    report.pending = unsynced_in(&mut conn).await?;
    Ok(report)
}

/// Turn supersession notices into prose. The winning value arrived in the same
/// response and has already been applied, so this only says why a value the
/// user was looking at changed underneath them.
async fn explain_lost(
    conn: &mut SqliteConnection,
    notices: &[Notice],
    batch: &[(EventId, CellEvent)],
    report: &mut SyncReport,
) -> Result<()> {
    let pushed: HashSet<String> = batch.iter().map(|(id, _)| id.to_string()).collect();
    for notice in notices {
        // The local row is gone: the entity was deleted, which is reported on
        // its own.
        if !row_exists(&mut *conn, &notice.winner.cell.kind, notice.entity_id).await? {
            continue;
        }
        // A queued edit of our own is the next word on that cell, so the loss
        // is not the user's latest intent.
        if pending_outranks(&mut *conn, &notice.winner, &pushed).await? {
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

async fn row_exists(conn: &mut SqliteConnection, kind: &str, id: Id) -> Result<bool> {
    let Some(table) = schema::table(kind) else {
        return Ok(false);
    };
    let row: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM \"{table}\" WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .with_context(|| format!("Failed to look up a {kind}"))?;
    Ok(row.is_some())
}

/// Events in one push: the server refuses more, and one push is one batch.
const MAX_PUSH: usize = 1000;

/// Read the outbox in emission order: a create's existence cell must reach the
/// server before the fields it introduces.
///
/// A push never splits a *create*. `create` appends the existence cell and its
/// fields back to back, the server gives a whole push one batch, and a page may
/// only break between batches. A reader materialises a row only from the page
/// that carries its existence cell, and requires that page to also carry the
/// fields the schema marks required (`insert_entity`); every later cell is a
/// blind `UPDATE`, so a create split across two pushes is lost on the reader for
/// good. The window therefore ends at an entity boundary: when the entity of the
/// last row continues past it, its rows wait for the next round. Later edits of
/// an already-created entity may split freely; the row exists by then and its
/// cells apply on their own.
///
/// Deferral is skipped only when the whole window belongs to the entity that
/// continues past it, which liveness needs: a single entity must not stall the
/// outbox. That cannot cut a create, because a create is one existence cell
/// plus at most its kind's fields — only repeated edits can fill a window.
async fn outbox(conn: &mut SqliteConnection) -> Result<Vec<(EventId, CellEvent)>> {
    let mut rows = sqlx::query(
        "SELECT id, event_id, device_id, entity_id, kind, field, value, timestamp
           FROM _sync_events ORDER BY id ASC LIMIT ?",
    )
    .bind(MAX_PUSH as i64)
    .fetch_all(&mut *conn)
    .await
    .context("Failed to read the outbox")?;
    if rows.len() == MAX_PUSH
        && let Some(last) = rows.last()
    {
        let tail = last.get::<String, _>("entity_id");
        let last_id = last.get::<i64, _>("id");
        let first = rows
            .iter()
            .position(|row| row.get::<String, _>("entity_id") == tail)
            .unwrap_or(0);
        let continues: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM _sync_events WHERE entity_id = ? AND id > ? LIMIT 1")
                .bind(&tail)
                .bind(last_id)
                .fetch_optional(&mut *conn)
                .await
                .context("Failed to read the outbox tail")?;
        if continues.is_some() && first > 0 {
            rows.truncate(first);
        }
    }
    let mut batch = Vec::with_capacity(rows.len());
    for row in rows {
        let value: Option<String> = row.get("value");
        batch.push((
            row.get("event_id"),
            CellEvent {
                event_id: row.get("event_id"),
                device_id: row.get("device_id"),
                entity_id: row.get("entity_id"),
                kind: row.get("kind"),
                field: row.get("field"),
                value: match value {
                    Some(json) => Some(
                        serde_json::from_str(&json)
                            .context("Failed to read a queued cell value")?,
                    ),
                    None => None,
                },
                timestamp: row.get("timestamp"),
            },
        ));
    }
    Ok(batch)
}

async fn unsynced_in(conn: &mut SqliteConnection) -> Result<usize> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sync_events")
        .fetch_one(&mut *conn)
        .await
        .context("Failed to count the outbox")?;
    Ok(count as usize)
}

/// The tracker slots this page touched, so slot collisions can be settled once.
async fn tracker_slots(
    conn: &mut SqliteConnection,
    slots: &TrackerSlots,
    rows: &[LogRow],
) -> Result<HashSet<(String, (i64, i64))>> {
    let mut touched = HashSet::new();
    for row in rows {
        if row.cell.kind != "tracker" || row.cell.field == EXISTENCE {
            continue;
        }
        let Some((tracker_type, time)) = apply::tracker_slot_key(&mut *conn, row.cell.entity_id).await?
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
    batch: &[(EventId, CellEvent)],
    now: i64,
    report: &mut SyncReport,
) -> Result<()> {
    let pushed: HashSet<String> = batch
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();
    for verdict in verdicts {
        match verdict.outcome {
            Outcome::Accepted => {}
            Outcome::Superseded => {
                let Some(winner) = &verdict.winner else {
                    continue;
                };
                if pending_outranks(conn, winner, &pushed).await? {
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
                    restamp(conn, verdict, batch).await?;
                    continue;
                }
                if let Some(tombstone) = &verdict.tombstone {
                    apply::apply_one(&mut *conn, tombstone).await?;
                }
                if let Some(event) = batch
                    .iter()
                    .map(|(_, event)| event)
                    .find(|event| event.event_id == verdict.event_id)
                {
                    if reason == "deleted" {
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
                        report.resurrect.push(Resurrect {
                            entity_id: event.entity_id,
                            kind: event.kind.clone(),
                            image: image_of(conn, verdict.event_id).await?.unwrap_or_default(),
                            device: verdict
                                .tombstone
                                .as_ref()
                                .map(|cell| cell.cell.device_id.to_string()),
                            at: verdict
                                .tombstone
                                .as_ref()
                                .map(|cell| cell.cell.timestamp),
                        });
                    } else if let Some(winner) = &verdict.winner {
                        apply::apply_one(&mut *conn, winner).await?;
                        report.notices.push(format!(
                            "{} refused ({reason}); showing {} instead",
                            describe(winner),
                            describe_value(&winner.cell.value),
                        ));
                    } else if event.field == EXISTENCE {
                        // A refused create leaves nothing behind.
                        apply::forget(&mut *conn, &event.kind, event.entity_id).await?;
                        report
                            .notices
                            .push(format!("refused ({reason}): {}", short(event.entity_id)));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether a local edit that was *not* part of this push outranks a served cell.
async fn pending_outranks(
    conn: &mut SqliteConnection,
    winner: &LogRow,
    pushed: &HashSet<String>,
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
        !pushed.contains(&event_id.to_string()) && timestamp > winner.cell.timestamp
    }))
}

/// Re-stamp a refused event: same change, fresh stamp and id.
async fn restamp(
    conn: &mut SqliteConnection,
    verdict: &Verdict,
    batch: &[(EventId, CellEvent)],
) -> Result<()> {
    if !batch.iter().any(|(_, event)| event.event_id == verdict.event_id) {
        return Ok(());
    }
    let stamp = state::next_stamp(&mut *conn).await?;
    sqlx::query("UPDATE _sync_events SET timestamp = ?, event_id = ? WHERE event_id = ?")
        .bind(stamp)
        .bind(EventId::new())
        .bind(verdict.event_id)
        .execute(&mut *conn)
        .await
        .context("Failed to re-stamp a refused event")?;
    Ok(())
}

/// The snapshot a delete event carried, if this device queued it.
async fn image_of(conn: &mut SqliteConnection, event_id: EventId) -> Result<Option<String>> {
    let image: Option<String> = sqlx::query_scalar("SELECT image FROM _sync_events WHERE event_id = ?")
        .bind(event_id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read a delete's snapshot")?
        .flatten();
    Ok(image)
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

/// Re-create a deleted entity from the snapshot its delete carried.
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
            if !referent_exists(&mut conn, link, target).await? {
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
    // The copy is built and written in one go, so nothing needs holding.
    let applied = apply::apply_rows(&mut tx, &rows, &mut Pending::default()).await?;
    if applied.created == 0 {
        return Ok(None);
    }
    match offer.kind.as_str() {
        "task" => super::events::task_create(&mut tx, entity).await?,
        "mood" => super::events::mood_create(&mut tx, entity).await?,
        _ => {}
    }
    tx.commit()
        .await
        .context("Failed to commit the resurrection")?;
    Ok(Some(entity))
}

async fn referent_exists(conn: &mut SqliteConnection, kind: &str, id: Id) -> Result<bool> {
    let Some(table) = schema::table(kind) else {
        return Ok(false);
    };
    let found: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM \"{table}\" WHERE id = ?"
    )))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to look up a {kind}"))?;
    Ok(found.is_some())
}

/// How many local changes are waiting to be pushed.
pub async fn unsynced_count(pool: &SqlitePool) -> Result<usize> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    unsynced_in(&mut conn).await
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

/// Create an account (blocking HTTP, off the async worker threads).
pub async fn register(base: &str, email: &str, password: &str) -> Result<Account> {
    let client = Client::new(base, None);
    let (email, password) = (email.to_string(), password.to_string());
    run_blocking(move || client.register(&email, &password)).await
}

/// Sign in to an existing account.
pub async fn login(base: &str, email: &str, password: &str) -> Result<Account> {
    let client = Client::new(base, None);
    let (email, password) = (email.to_string(), password.to_string());
    run_blocking(move || client.login(&email, &password)).await
}

/// The account a stored token belongs to.
pub async fn account_status(base: &str, token: &str) -> Result<AccountStatus> {
    let client = Client::new(base, Some(token.to_string()));
    run_blocking(move || client.status()).await
}
