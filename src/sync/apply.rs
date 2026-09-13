//! Replay of incoming events: field-level last-write-wins with the promptable
//! conflicts of `@@SYNC.md` §4.
//!
//! A field is written only when its event beats the stamp recorded for that
//! field, so two devices that edited *different* fields of one row keep both
//! edits (§4.1) while a shared field converges. Contradictions with a local
//! decision that is still unsynced become a [`Conflict`]: the caller asks the
//! user and [`resolve_conflict`] turns the answer into an event, so every
//! device converges and prompt loops terminate (§4.2).

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::db::Id;
use crate::tracker::TrackerSlots;

use super::events;
use super::state::{self, Stamp};
use super::types::{
    Change, EntityPayload, RemoteEvent, TrackerScore, completion_field, fields, is_text_field,
    mood_field, task_field, tracker_field,
};

/// How many parent hops a cycle check follows before giving up: a loop already
/// in the table must not hang the replay.
const MAX_PARENT_HOPS: usize = 64;

/// Why an incoming event needs the user's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// A delete arrived for an entity this device edited locally, while those
    /// edits are still unsynced (§4.2.2).
    RemoteDeleteVsLocalEdit,
    /// An update arrived for an entity this device deleted locally, while that
    /// deletion is still unsynced (§4.2.2).
    LocalDeleteVsRemoteUpdate,
    /// A completion arrived for a task this device does not have (§4.2.2).
    CompletionOnMissingTask,
    /// Both devices replaced the same text field (§4.2.1).
    TextReplaced,
    /// The incoming parent link closes a hierarchy cycle (§4.2.3).
    ParentCycle,
}

/// The user's decision on a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Confirm the deletion, dropping the other side's change.
    ConfirmDeletion,
    /// Restore the entity (from its snapshot / with this device's values).
    Resurrect,
    /// Keep this device's value for the conflicting field.
    KeepLocal,
    /// Take the other device's value for the conflicting field.
    KeepRemote,
    /// Keep both notes, this device's first (body conflicts).
    AppendBoth,
    /// Use the parent link that arrived.
    UseRemoteParent,
    /// Keep this device's parent link.
    UseLocalParent,
    /// Detach both tasks to the root (parent cycles).
    DetachBoth,
}

/// A contradiction between an incoming event and a decision this device has
/// not pushed yet.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub entity_id: Id,
    /// The event that triggered the conflict.
    pub event: RemoteEvent,
    /// The field in conflict, for text conflicts and cycles.
    pub field: Option<&'static str>,
    /// This device's value of the conflicting field.
    pub local_value: Option<String>,
    /// The incoming value of the conflicting field.
    pub remote_value: Option<String>,
    /// The local and incoming parent task (parent cycles).
    pub local_parent: Option<Id>,
    pub remote_parent: Option<Id>,
    /// Whether the entity can be restored from its snapshot.
    pub resurrectable: bool,
}

/// What applying one event did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// The event wrote to the materialized tables (a field that lost to its
    /// own watermark changes nothing).
    pub wrote: bool,
    /// Contradictions left to the user (see [`resolve_conflict`]).
    pub conflicts: Vec<Conflict>,
}

/// What applying one pulled page did.
#[derive(Debug, Clone, Default)]
pub struct PageOutcome {
    /// Events that wrote something.
    pub applied: usize,
    /// Events that lost to a watermark.
    pub stale: usize,
    /// Contradictions left to the user.
    pub conflicts: Vec<Conflict>,
}

impl ConflictKind {
    pub fn describe(&self) -> &'static str {
        match self {
            ConflictKind::RemoteDeleteVsLocalEdit => {
                "another device deleted an entry this device edited"
            }
            ConflictKind::LocalDeleteVsRemoteUpdate => {
                "another device edited an entry this device deleted"
            }
            ConflictKind::CompletionOnMissingTask => {
                "a completion arrived for a task this device does not have"
            }
            ConflictKind::TextReplaced => "both devices replaced the same text",
            ConflictKind::ParentCycle => "the two tasks would become each other's parent",
        }
    }

    /// The choices this conflict offers, in prompt order (§4.2).
    pub fn options(&self, conflict: &Conflict) -> Vec<(Resolution, &'static str, &'static str)> {
        match self {
            ConflictKind::RemoteDeleteVsLocalEdit => vec![
                (
                    Resolution::ConfirmDeletion,
                    "Confirm the deletion",
                    "discard this device's edits",
                ),
                (
                    Resolution::Resurrect,
                    "Resurrect the entry",
                    "keep this device's values",
                ),
            ],
            ConflictKind::LocalDeleteVsRemoteUpdate => vec![
                (
                    Resolution::ConfirmDeletion,
                    "Keep the deletion",
                    "discard the other device's edit",
                ),
                (
                    Resolution::KeepRemote,
                    "Take the edit",
                    "resurrect with the other device's values",
                ),
            ],
            ConflictKind::CompletionOnMissingTask => vec![
                (
                    Resolution::ConfirmDeletion,
                    "Drop the completion",
                    "the task stays deleted",
                ),
                (
                    Resolution::Resurrect,
                    "Resurrect the task",
                    "restore the task and keep the completion",
                ),
            ],
            ConflictKind::TextReplaced => {
                let mut options = vec![
                    (Resolution::KeepLocal, "Keep this device's text", ""),
                    (Resolution::KeepRemote, "Take the other device's text", ""),
                ];
                // Only a note can hold both texts; a name or a mood label is
                // one value or the other.
                if conflict.field == Some(mood_field::BODY) {
                    options.push((
                        Resolution::AppendBoth,
                        "Append both",
                        "this device's text first",
                    ));
                }
                options
            }
            ConflictKind::ParentCycle => vec![
                (Resolution::UseRemoteParent, "Use the incoming parent", ""),
                (Resolution::UseLocalParent, "Keep this device's parent", ""),
                (
                    Resolution::DetachBoth,
                    "Detach both to the root",
                    "neither task has a parent",
                ),
            ],
        }
    }
}

/// Apply one pulled page: every event in dependency order, the tracker slot
/// cleanup and the pull cursor — all in a single transaction (§4.3).
pub async fn apply_page(
    pool: &SqlitePool,
    events: &[RemoteEvent],
    cursor: i64,
    slots: &TrackerSlots,
) -> Result<PageOutcome> {
    let mut tx = pool.begin().await.context("Failed to begin a sync apply")?;
    let mut outcome = PageOutcome::default();
    // The slots that received a tracker entry, for the cleanup below.
    let mut touched: HashSet<(String, (i64, i64))> = HashSet::new();
    let mut tasks: Vec<Id> = Vec::new();

    for remote in replay_order(events) {
        let step = apply_one(&mut tx, remote).await?;
        if step.wrote {
            outcome.applied += 1;
            match &remote.event.payload {
                Some(EntityPayload::TaskCreate(_) | EntityPayload::TaskUpdate(_)) => {
                    tasks.push(remote.event.entity_id);
                }
                Some(EntityPayload::TrackerCreate(_) | EntityPayload::TrackerUpdate(_)) => {
                    mark_tracker_slot(&mut tx, remote.event.entity_id, slots, &mut touched).await?;
                }
                _ => {}
            }
        } else if step.conflicts.is_empty() {
            outcome.stale += 1;
        }
        outcome.conflicts.extend(step.conflicts);
    }

    dedup_tracker_slots(&mut tx, &touched).await?;
    for entity_id in tasks {
        crate::db::sync_short_id(&mut tx, entity_id).await?;
    }
    state::set(&mut tx, state::KEY_LAST_SERVER_VERSION, &cursor.to_string()).await?;
    tx.commit().await.context("Failed to commit a sync apply")?;
    Ok(outcome)
}

/// Apply a single event, outside the paged replay: no cursor and no slot
/// cleanup. Used by the resolution flow and the engine tests.
pub async fn apply_event(pool: &SqlitePool, remote: &RemoteEvent) -> Result<Applied> {
    let mut tx = pool.begin().await.context("Failed to begin a sync apply")?;
    let step = apply_one(&mut tx, remote).await?;
    if step.wrote {
        if let Some(EntityPayload::TaskCreate(_) | EntityPayload::TaskUpdate(_)) =
            &remote.event.payload
        {
            crate::db::sync_short_id(&mut tx, remote.event.entity_id).await?;
        }
        tx.commit().await.context("Failed to commit a sync apply")?;
    }
    Ok(step)
}

/// Apply one event to an open transaction.
async fn apply_one(conn: &mut SqliteConnection, remote: &RemoteEvent) -> Result<Applied> {
    let entity_id = remote.event.entity_id;
    let incoming = Stamp::from(remote);
    let known = state::entity(&mut *conn, entity_id).await?;
    match &remote.event.payload {
        None => apply_delete(conn, remote, &incoming, known).await,
        Some(payload) => apply_mutation(conn, remote, payload, &incoming, known).await,
    }
}

/// The order a page is applied in: creations and updates by dependency (a task
/// before the moods, trackers and completions that reference it), then deletes.
/// Deletes stay in log order — removing a row cascades or nulls its
/// references, so a delete cannot break a foreign key whatever the order.
fn replay_order(events: &[RemoteEvent]) -> Vec<&RemoteEvent> {
    fn rank(payload: &Option<EntityPayload>) -> u8 {
        match payload {
            Some(EntityPayload::TaskCreate(_) | EntityPayload::TaskUpdate(_)) => 0,
            Some(EntityPayload::MoodCreate(_) | EntityPayload::MoodUpdate(_)) => 1,
            Some(EntityPayload::TrackerCreate(_) | EntityPayload::TrackerUpdate(_)) => 2,
            Some(EntityPayload::Completion(_)) => 3,
            None => 4,
        }
    }
    let mut ordered: Vec<&RemoteEvent> = events.iter().collect();
    ordered.sort_by_key(|remote| (rank(&remote.event.payload), remote.version));
    ordered
}

async fn apply_delete(
    conn: &mut SqliteConnection,
    remote: &RemoteEvent,
    incoming: &Stamp,
    known: Option<state::Entity>,
) -> Result<Applied> {
    let entity_id = remote.event.entity_id;
    let kind = known
        .as_ref()
        .map(|entity| entity.kind.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // LWW first: the deletion changes nothing when a decision of this device
    // already outranks it.
    if let Some(current) = state::watermark(&mut *conn, entity_id, state::ENTITY).await?
        && !state::wins(incoming, &current)
    {
        return Ok(Applied::default());
    }

    let alive = known.as_ref().is_some_and(|entity| !entity.deleted);
    if alive && state::has_pending(&mut *conn, entity_id).await? {
        return Ok(Applied {
            wrote: false,
            conflicts: vec![Conflict {
                kind: ConflictKind::RemoteDeleteVsLocalEdit,
                entity_id,
                event: remote.clone(),
                field: None,
                local_value: None,
                remote_value: None,
                local_parent: None,
                remote_parent: None,
                resurrectable: true,
            }],
        });
    }

    commit_delete(conn, entity_id, &kind, incoming).await?;
    Ok(Applied {
        wrote: true,
        conflicts: Vec::new(),
    })
}

/// Drop the row and record the deletion as the winner of every field, keeping
/// the field values so a resurrection has a snapshot (§4.2.2).
async fn commit_delete(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    stamp: &Stamp,
) -> Result<()> {
    if let Some(table) = entity_table(kind) {
        let sql = format!("DELETE FROM {table} WHERE id = ?");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(entity_id)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("Failed to delete a synced {table} row"))?;
    }
    for field in fields(kind) {
        state::record_stamp_keep_value(&mut *conn, entity_id, field, stamp).await?;
    }
    state::record(&mut *conn, entity_id, state::ENTITY, stamp, None).await?;
    state::set_entity(&mut *conn, entity_id, kind, true).await?;
    Ok(())
}

async fn apply_mutation(
    conn: &mut SqliteConnection,
    remote: &RemoteEvent,
    payload: &EntityPayload,
    incoming: &Stamp,
    known: Option<state::Entity>,
) -> Result<Applied> {
    let entity_id = remote.event.entity_id;
    let kind = payload.kind();

    // A completion may not reference a task that is gone here (§4.2.2).
    if let EntityPayload::Completion(data) = payload
        && !entity_exists(&mut *conn, data.todo_id).await?
    {
        let snapshot = !state::values(&mut *conn, data.todo_id).await?.is_empty();
        return Ok(Applied {
            wrote: false,
            conflicts: vec![Conflict {
                kind: ConflictKind::CompletionOnMissingTask,
                entity_id,
                event: remote.clone(),
                field: None,
                local_value: None,
                remote_value: None,
                local_parent: None,
                remote_parent: None,
                resurrectable: snapshot,
            }],
        });
    }

    if known.as_ref().is_some_and(|entity| entity.deleted) {
        if state::pending_delete(&mut *conn, entity_id).await? {
            return Ok(Applied {
                wrote: false,
                conflicts: vec![Conflict {
                    kind: ConflictKind::LocalDeleteVsRemoteUpdate,
                    entity_id,
                    event: remote.clone(),
                    field: None,
                    local_value: None,
                    remote_value: None,
                    local_parent: None,
                    remote_parent: None,
                    resurrectable: true,
                }],
            });
        }
        // The deletion is acknowledged: LWW decides, and a mutation above it
        // resurrects the entity from the snapshot.
        if let Some(delete_stamp) = state::watermark(&mut *conn, entity_id, state::ENTITY).await?
            && !state::wins(incoming, &delete_stamp)
        {
            return Ok(Applied::default());
        }
    }

    let mut values = current_values(&mut *conn, entity_id, kind).await?;
    let pending = state::pending_fields(&mut *conn, entity_id).await?;
    let mut conflicts = Vec::new();
    let mut wrote = false;

    for change in payload.changes() {
        // Concurrent replacements of a text field are the user's call (§4.2.1).
        if is_text_field(kind, change.field)
            && pending.contains(change.field)
            && values.get(change.field) != change.value.as_ref()
        {
            conflicts.push(Conflict {
                kind: ConflictKind::TextReplaced,
                entity_id,
                event: remote.clone(),
                field: Some(change.field),
                local_value: text_of(values.get(change.field)),
                remote_value: text_of(change.value.as_ref()),
                local_parent: None,
                remote_parent: None,
                resurrectable: true,
            });
            continue;
        }

        if let Some(current) = state::watermark(&mut *conn, entity_id, change.field).await?
            && !state::wins(incoming, &current)
        {
            continue;
        }

        // A parent link that closes a cycle is repaired deterministically, or
        // asked about when this device decided the other link itself (§4.2.3).
        if kind == "task"
            && change.field == task_field::PARENT_ID
            && let Some(parent) = change.value.as_ref().and_then(as_id)
            && let Some(ancestor) = closes_cycle(&mut *conn, entity_id, parent).await?
        {
            let local_decided = pending.contains(task_field::PARENT_ID)
                || !state::pending_fields(&mut *conn, ancestor)
                    .await?
                    .is_empty();
            if local_decided {
                conflicts.push(Conflict {
                    kind: ConflictKind::ParentCycle,
                    entity_id,
                    event: remote.clone(),
                    field: Some(task_field::PARENT_ID),
                    local_value: None,
                    remote_value: None,
                    local_parent: values.get(task_field::PARENT_ID).and_then(as_id),
                    remote_parent: Some(parent),
                    resurrectable: true,
                });
                continue;
            }
            // Nobody here decided either link: the later link wins and the
            // older one is detached, on every device alike.
            if let Some(other) =
                state::watermark(&mut *conn, ancestor, task_field::PARENT_ID).await?
                && !state::wins(incoming, &other)
            {
                continue;
            }
            detach_parent(&mut *conn, ancestor, incoming).await?;
        }

        set_value(&mut values, &change);
        state::record(
            &mut *conn,
            entity_id,
            change.field,
            incoming,
            change.value.as_ref(),
        )
        .await?;
        wrote = true;
    }

    if wrote {
        // Links to rows that are gone here become NULL, exactly as SQLite's
        // `ON DELETE SET NULL` would have left them (§4.1).
        drop_missing_links(&mut *conn, kind, &mut values).await?;
        write_values(&mut *conn, entity_id, kind, &values).await?;
        state::record(&mut *conn, entity_id, state::ENTITY, incoming, None).await?;
        state::set_entity(&mut *conn, entity_id, kind, false).await?;
    }
    Ok(Applied { wrote, conflicts })
}

// ---------------------------------------------------------------------------
// Resolutions
// ---------------------------------------------------------------------------

/// Resolve a conflict the user was prompted for. The decision is written as an
/// event so every device converges on it.
pub async fn resolve_conflict(
    pool: &SqlitePool,
    conflict: &Conflict,
    resolution: Resolution,
) -> Result<()> {
    let entity_id = conflict.entity_id;
    let mut tx = pool.begin().await.context("Failed to begin a resolution")?;
    let kind = match state::entity(&mut tx, entity_id).await? {
        Some(entity) => entity.kind,
        None => conflict_kind_of(conflict),
    };

    match (conflict.kind, resolution) {
        (ConflictKind::RemoteDeleteVsLocalEdit, Resolution::ConfirmDeletion) => {
            // The deletion stands; the local edits are dropped.
            commit_delete(&mut tx, entity_id, &kind, &Stamp::from(&conflict.event)).await?;
            mark_events_synced(&mut tx, entity_id).await?;
        }
        (ConflictKind::RemoteDeleteVsLocalEdit, Resolution::Resurrect) => {
            // Acknowledge the deletion, then publish this device's values above
            // it so every peer resurrects the entry.
            record_event_stamps(&mut tx, conflict).await?;
            events::republish(&mut tx, entity_id, &kind).await?;
        }
        (ConflictKind::LocalDeleteVsRemoteUpdate, Resolution::ConfirmDeletion) => {
            mark_events_synced(&mut tx, entity_id).await?;
            events::delete(&mut tx, entity_id, &kind).await?;
        }
        (ConflictKind::LocalDeleteVsRemoteUpdate, Resolution::KeepRemote) => {
            // Take the other device's values, then publish them above this
            // device's deletion.
            let payload = conflict
                .event
                .event
                .payload
                .as_ref()
                .context("an update conflict must carry a mutation")?
                .clone();
            apply_remote_force(
                &mut tx,
                entity_id,
                &kind,
                &payload,
                &Stamp::from(&conflict.event),
            )
            .await?;
            mark_events_synced(&mut tx, entity_id).await?;
            events::republish(&mut tx, entity_id, &kind).await?;
        }
        (ConflictKind::CompletionOnMissingTask, Resolution::ConfirmDeletion) => {
            mark_events_synced(&mut tx, entity_id).await?;
            events::delete(&mut tx, entity_id, "completion").await?;
        }
        (ConflictKind::CompletionOnMissingTask, Resolution::Resurrect) => {
            let todo_id = completion_todo_id(conflict)?;
            let snapshot: Map<String, Value> =
                state::values(&mut tx, todo_id).await?.into_iter().collect();
            anyhow::ensure!(
                !snapshot.is_empty(),
                "no snapshot of the deleted task is available"
            );
            let mut snapshot = snapshot;
            drop_missing_links(&mut tx, "task", &mut snapshot).await?;
            write_values(&mut tx, todo_id, "task", &snapshot).await?;
            state::set_entity(&mut tx, todo_id, "task", false).await?;
            events::republish(&mut tx, todo_id, "task").await?;
            apply_remote_force(
                &mut tx,
                entity_id,
                "completion",
                conflict
                    .event
                    .event
                    .payload
                    .as_ref()
                    .context("no payload")?,
                &Stamp::from(&conflict.event),
            )
            .await?;
            mark_events_synced(&mut tx, entity_id).await?;
            crate::db::sync_short_id(&mut tx, todo_id).await?;
        }
        (ConflictKind::TextReplaced, Resolution::KeepLocal) => {
            // Re-publish this device's text above the incoming one.
            let field = conflict
                .field
                .context("a text conflict carries its field")?;
            record_event_stamps(&mut tx, conflict).await?;
            let change = Change::set(field, conflict.local_value.clone().unwrap_or_default());
            events::republish_field(&mut tx, entity_id, &kind, &change).await?;
        }
        (ConflictKind::TextReplaced, Resolution::KeepRemote) => {
            // The incoming text already loses to nothing: apply it.
            let field = conflict
                .field
                .context("a text conflict carries its field")?;
            let value = conflict.remote_value.clone().map(Value::String);
            write_field(
                &mut tx,
                entity_id,
                &kind,
                field,
                value.as_ref(),
                &Stamp::from(&conflict.event),
            )
            .await?;
        }
        (ConflictKind::TextReplaced, Resolution::AppendBoth) => {
            let field = conflict
                .field
                .context("a text conflict carries its field")?;
            let local = conflict.local_value.clone().unwrap_or_default();
            let remote = conflict.remote_value.clone().unwrap_or_default();
            let merged = format!("{local}\n{remote}");
            record_event_stamps(&mut tx, conflict).await?;
            let change = Change::set(field, &merged);
            write_field(
                &mut tx,
                entity_id,
                &kind,
                field,
                change.value.as_ref(),
                &Stamp::from(&conflict.event),
            )
            .await?;
            events::republish_field(&mut tx, entity_id, &kind, &change).await?;
        }
        (ConflictKind::ParentCycle, resolution) => {
            resolve_cycle(&mut tx, conflict, resolution).await?;
        }
        (kind, resolution) => {
            anyhow::bail!("{kind:?} cannot be resolved with {resolution:?}");
        }
    }
    tx.commit().await.context("Failed to commit a resolution")?;
    Ok(())
}

/// Settle a parent cycle: the chosen link stays, the other is detached, and
/// both choices are published so peers that applied the other link follow.
async fn resolve_cycle(
    conn: &mut SqliteConnection,
    conflict: &Conflict,
    resolution: Resolution,
) -> Result<()> {
    let entity_id = conflict.entity_id;
    let ancestor = ancestor_closing_cycle(conn, entity_id, conflict.remote_parent).await?;
    let incoming = Stamp::from(&conflict.event);
    match resolution {
        Resolution::UseRemoteParent => {
            record_event_stamps(conn, conflict).await?;
            if let Some(parent) = conflict.remote_parent {
                let change = Change::set(task_field::PARENT_ID, parent);
                write_field(
                    conn,
                    entity_id,
                    "task",
                    task_field::PARENT_ID,
                    change.value.as_ref(),
                    &incoming,
                )
                .await?;
                events::republish_field(conn, entity_id, "task", &change).await?;
            }
            if let Some(ancestor) = ancestor {
                detach_parent(conn, ancestor, &incoming).await?;
                events::republish_field(
                    conn,
                    ancestor,
                    "task",
                    &Change::clear(task_field::PARENT_ID),
                )
                .await?;
            }
        }
        Resolution::UseLocalParent => {
            // Keep this device's link and publish it, so peers that applied the
            // incoming link revert.
            let local = conflict.local_parent;
            let change = match local {
                Some(parent) => Change::set(task_field::PARENT_ID, parent),
                None => Change::clear(task_field::PARENT_ID),
            };
            write_field(
                conn,
                entity_id,
                "task",
                task_field::PARENT_ID,
                change.value.as_ref(),
                &incoming,
            )
            .await?;
            events::republish_field(conn, entity_id, "task", &change).await?;
            record_event_stamps(conn, conflict).await?;
        }
        Resolution::DetachBoth => {
            record_event_stamps(conn, conflict).await?;
            for task in [Some(entity_id), ancestor].into_iter().flatten() {
                let change = Change::clear(task_field::PARENT_ID);
                write_field(conn, task, "task", task_field::PARENT_ID, None, &incoming).await?;
                events::republish_field(conn, task, "task", &change).await?;
            }
        }
        other => anyhow::bail!("{other:?} does not settle a parent cycle"),
    }
    Ok(())
}

/// Record the incoming event's stamps for the entity and its fields, without
/// writing the row: the acknowledgement that lets the compensating event this
/// device publishes outrank it.
async fn record_event_stamps(conn: &mut SqliteConnection, conflict: &Conflict) -> Result<()> {
    let stamp = Stamp::from(&conflict.event);
    let entity_id = conflict.entity_id;
    if let Some(payload) = &conflict.event.event.payload {
        for change in payload.changes() {
            state::record(
                &mut *conn,
                entity_id,
                change.field,
                &stamp,
                change.value.as_ref(),
            )
            .await?;
        }
    }
    state::record(&mut *conn, entity_id, state::ENTITY, &stamp, None).await?;
    Ok(())
}

/// Materialize a remote mutation over whatever this device has, ignoring the
/// watermarks: the decision was "take the other device's values".
async fn apply_remote_force(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    payload: &EntityPayload,
    stamp: &Stamp,
) -> Result<()> {
    let mut values = current_values(&mut *conn, entity_id, kind).await?;
    for change in payload.changes() {
        set_value(&mut values, &change);
        state::record(
            &mut *conn,
            entity_id,
            change.field,
            stamp,
            change.value.as_ref(),
        )
        .await?;
    }
    drop_missing_links(&mut *conn, kind, &mut values).await?;
    write_values(&mut *conn, entity_id, kind, &values).await?;
    state::record(&mut *conn, entity_id, state::ENTITY, stamp, None).await?;
    state::set_entity(&mut *conn, entity_id, kind, false).await?;
    Ok(())
}

/// Write one field of an entity and record it as the winner.
async fn write_field(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    field: &str,
    value: Option<&Value>,
    stamp: &Stamp,
) -> Result<()> {
    let mut values = current_values(&mut *conn, entity_id, kind).await?;
    values.insert(field.to_string(), value.cloned().unwrap_or(Value::Null));
    drop_missing_links(&mut *conn, kind, &mut values).await?;
    write_values(&mut *conn, entity_id, kind, &values).await?;
    state::record(&mut *conn, entity_id, field, stamp, value).await?;
    state::record(&mut *conn, entity_id, state::ENTITY, stamp, None).await?;
    state::set_entity(&mut *conn, entity_id, kind, false).await?;
    Ok(())
}

/// Stop pushing the queued events of an entity: the user's decision superseded
/// them.
async fn mark_events_synced(conn: &mut SqliteConnection, entity_id: Id) -> Result<()> {
    sqlx::query("UPDATE _sync_events SET synced = 1 WHERE entity_id = ? AND synced = 0")
        .bind(entity_id)
        .execute(&mut *conn)
        .await
        .context("Failed to settle the sync outbox")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// The current values of every field of an entity: its row, or the watermark
/// snapshot when the row is gone (a resurrection, §4.2.2).
async fn current_values(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
) -> Result<Map<String, Value>> {
    let row = match kind {
        "task" => sqlx::query(
            "SELECT name, body, priority, start_time, available_duration_secs, interval_secs,
                        target_count, optional, end_time, parent
                 FROM todos WHERE id = ?",
        )
        .bind(entity_id)
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| task_values(&row)),
        "mood" => {
            sqlx::query("SELECT mood, body, time, score, duration, todo_id FROM mood WHERE id = ?")
                .bind(entity_id)
                .fetch_optional(&mut *conn)
                .await?
                .map(|row| mood_values(&row))
        }
        "tracker" => sqlx::query(
            "SELECT type, typeof(score) AS score_kind, CAST(score AS TEXT) AS score_text,
                        time, mood FROM tracker WHERE id = ?",
        )
        .bind(entity_id)
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| tracker_values(&row)),
        "completion" => {
            sqlx::query("SELECT todo_id, time, count FROM todo_completions WHERE id = ?")
                .bind(entity_id)
                .fetch_optional(&mut *conn)
                .await?
                .map(|row| {
                    Map::from_iter([
                        (
                            completion_field::TODO_ID.to_string(),
                            json_id(row.get("todo_id")),
                        ),
                        (
                            completion_field::TIME.to_string(),
                            Value::from(row.get::<i64, _>("time")),
                        ),
                        (
                            completion_field::COUNT.to_string(),
                            Value::from(row.get::<i32, _>("count")),
                        ),
                    ])
                })
        }
        _ => None,
    };
    match row {
        Some(values) => Ok(values),
        None => Ok(state::values(&mut *conn, entity_id)
            .await?
            .into_iter()
            .collect()),
    }
}

fn task_values(row: &sqlx::sqlite::SqliteRow) -> Map<String, Value> {
    Map::from_iter([
        (
            task_field::NAME.to_string(),
            row.get::<String, _>("name").into(),
        ),
        (
            task_field::BODY.to_string(),
            row.get::<String, _>("body").into(),
        ),
        (
            task_field::PRIORITY.to_string(),
            Value::from(row.get::<i32, _>("priority")),
        ),
        (
            task_field::START_TIME.to_string(),
            json_opt(row.get::<Option<i64>, _>("start_time")),
        ),
        (
            task_field::AVAILABLE_DURATION_SECS.to_string(),
            json_opt(row.get::<Option<i64>, _>("available_duration_secs")),
        ),
        (
            task_field::INTERVAL_SECS.to_string(),
            json_opt(row.get::<Option<i64>, _>("interval_secs")),
        ),
        (
            task_field::TARGET_COUNT.to_string(),
            Value::from(row.get::<i32, _>("target_count")),
        ),
        (
            task_field::OPTIONAL.to_string(),
            Value::from(row.get::<i32, _>("optional") != 0),
        ),
        (
            task_field::END_TIME.to_string(),
            json_opt(row.get::<Option<i64>, _>("end_time")),
        ),
        (
            task_field::PARENT_ID.to_string(),
            json_id(row.get::<Option<Id>, _>("parent")),
        ),
    ])
}

fn mood_values(row: &sqlx::sqlite::SqliteRow) -> Map<String, Value> {
    Map::from_iter([
        (
            mood_field::MOOD.to_string(),
            row.get::<String, _>("mood").into(),
        ),
        (
            mood_field::BODY.to_string(),
            row.get::<String, _>("body").into(),
        ),
        (
            mood_field::TIME.to_string(),
            Value::from(row.get::<i64, _>("time")),
        ),
        (
            mood_field::SCORE.to_string(),
            json_opt(row.get::<Option<f32>, _>("score")),
        ),
        (
            mood_field::DURATION.to_string(),
            json_opt(row.get::<Option<i64>, _>("duration")),
        ),
        (
            mood_field::TODO_ID.to_string(),
            json_id(row.get::<Option<Id>, _>("todo_id")),
        ),
    ])
}

fn tracker_values(row: &sqlx::sqlite::SqliteRow) -> Map<String, Value> {
    let score =
        crate::db::tracker_value(row.get("score_kind"), &row.get::<String, _>("score_text"));
    Map::from_iter([
        (
            tracker_field::TRACKER_TYPE.to_string(),
            row.get::<String, _>("type").into(),
        ),
        (
            tracker_field::SCORE.to_string(),
            serde_json::to_value(TrackerScore::from(&score)).expect("a score serializes"),
        ),
        (
            tracker_field::TIME.to_string(),
            Value::from(row.get::<i64, _>("time")),
        ),
        (
            tracker_field::MOOD_ID.to_string(),
            json_id(row.get::<Option<Id>, _>("mood")),
        ),
    ])
}

/// Insert/update the row an entity's values describe.
async fn write_values(
    conn: &mut SqliteConnection,
    entity_id: Id,
    kind: &str,
    values: &Map<String, Value>,
) -> Result<()> {
    match kind {
        "task" => {
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
            .bind(text(values, task_field::NAME))
            .bind(text(values, task_field::BODY))
            .bind(integer(values, task_field::PRIORITY).unwrap_or(5) as i32)
            .bind(integer(values, task_field::START_TIME))
            .bind(integer(values, task_field::AVAILABLE_DURATION_SECS))
            .bind(integer(values, task_field::INTERVAL_SECS))
            .bind(integer(values, task_field::TARGET_COUNT).unwrap_or(0) as i32)
            .bind(flag(values, task_field::OPTIONAL) as i32)
            .bind(integer(values, task_field::END_TIME))
            .bind(id_of(values, task_field::PARENT_ID))
            .execute(&mut *conn)
            .await
            .context("Failed to write a synced task")?;
        }
        "mood" => {
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
            .bind(text(values, mood_field::MOOD))
            .bind(text(values, mood_field::BODY))
            .bind(integer(values, mood_field::TIME).unwrap_or_default())
            .bind(number(values, mood_field::SCORE))
            .bind(integer(values, mood_field::DURATION))
            .bind(id_of(values, mood_field::TODO_ID))
            .execute(&mut *conn)
            .await
            .context("Failed to write a synced mood")?;
        }
        "tracker" => {
            let score: TrackerScore = match values.get(tracker_field::SCORE) {
                Some(value) => {
                    serde_json::from_value(value.clone()).context("Corrupt tracker score")?
                }
                None => TrackerScore::Integer(0),
            };
            let mut query = sqlx::query(
                "INSERT INTO tracker (id, type, score, time, mood) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     type = excluded.type, score = excluded.score, time = excluded.time,
                     mood = excluded.mood",
            )
            .bind(entity_id)
            .bind(text(values, tracker_field::TRACKER_TYPE));
            query = match score {
                TrackerScore::Text(value) => query.bind(value),
                TrackerScore::Integer(value) => query.bind(value),
                TrackerScore::Float(value) => query.bind(value),
            };
            query
                .bind(integer(values, tracker_field::TIME).unwrap_or_default())
                .bind(id_of(values, tracker_field::MOOD_ID))
                .execute(&mut *conn)
                .await
                .context("Failed to write a synced tracker entry")?;
        }
        "completion" => {
            sqlx::query(
                "INSERT INTO todo_completions (id, todo_id, time, count) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     todo_id = excluded.todo_id, time = excluded.time, count = excluded.count",
            )
            .bind(entity_id)
            .bind(id_of(values, completion_field::TODO_ID))
            .bind(integer(values, completion_field::TIME).unwrap_or_default())
            .bind(integer(values, completion_field::COUNT).unwrap_or(1) as i32)
            .execute(&mut *conn)
            .await
            .context("Failed to write a synced completion")?;
        }
        other => anyhow::bail!("unknown synced entity kind '{other}'"),
    }
    Ok(())
}

/// A link to a row this device does not have becomes NULL, exactly as
/// `ON DELETE SET NULL` would have left it (§4.1: a subtask of a deleted
/// parent is promoted to the root).
async fn drop_missing_links(
    conn: &mut SqliteConnection,
    kind: &str,
    values: &mut Map<String, Value>,
) -> Result<()> {
    let field = match kind {
        "task" => task_field::PARENT_ID,
        "mood" => mood_field::TODO_ID,
        "tracker" => tracker_field::MOOD_ID,
        _ => return Ok(()),
    };
    if let Some(link) = id_of(values, field)
        && !entity_exists(&mut *conn, link).await?
    {
        values.insert(field.to_string(), Value::Null);
    }
    Ok(())
}

/// Whether this device materialized the entity (a live row in any table).
async fn entity_exists(conn: &mut SqliteConnection, id: Id) -> Result<bool> {
    for table in TABLES {
        let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?)");
        let found: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("Failed to look up id in {table}"))?;
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The tables an entity id can live in.
const TABLES: [&str; 4] = ["mood", "tracker", "todos", "todo_completions"];

fn entity_table(kind: &str) -> Option<&'static str> {
    match kind {
        "task" => Some("todos"),
        "mood" => Some("mood"),
        "tracker" => Some("tracker"),
        "completion" => Some("todo_completions"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

/// The task whose parent link closes the cycle if `entity` becomes a child of
/// `parent`: walking up from `parent` must never reach `entity` (§4.2.3).
async fn closes_cycle(conn: &mut SqliteConnection, entity: Id, parent: Id) -> Result<Option<Id>> {
    if parent == entity {
        return Ok(Some(entity));
    }
    let mut seen = HashSet::from([entity]);
    let mut cursor = Some(parent);
    for _ in 0..MAX_PARENT_HOPS {
        let Some(current) = cursor else {
            return Ok(None);
        };
        if !seen.insert(current) {
            return Ok(None);
        }
        let next: Option<Id> = sqlx::query_scalar("SELECT parent FROM todos WHERE id = ?")
            .bind(current)
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to walk the task hierarchy")?
            .flatten();
        match next {
            Some(next) if next == entity => return Ok(Some(current)),
            other => cursor = other,
        }
    }
    Ok(None)
}

/// The ancestor that points back at `entity` for a reported cycle.
async fn ancestor_closing_cycle(
    conn: &mut SqliteConnection,
    entity: Id,
    remote_parent: Option<Id>,
) -> Result<Option<Id>> {
    let Some(parent) = remote_parent else {
        return Ok(None);
    };
    closes_cycle(conn, entity, parent).await
}

/// Detach one task from its parent without emitting anything: the deterministic
/// repair both devices of a cycle reach on their own.
async fn detach_parent(conn: &mut SqliteConnection, task: Id, stamp: &Stamp) -> Result<()> {
    sqlx::query("UPDATE todos SET parent = NULL WHERE id = ?")
        .bind(task)
        .execute(&mut *conn)
        .await
        .context("Failed to detach a task from its parent")?;
    state::record(&mut *conn, task, task_field::PARENT_ID, stamp, None).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tracker slots
// ---------------------------------------------------------------------------

/// Remember the slot the tracker entry just landed in.
async fn mark_tracker_slot(
    conn: &mut SqliteConnection,
    entity_id: Id,
    slots: &TrackerSlots,
    touched: &mut HashSet<(String, (i64, i64))>,
) -> Result<()> {
    let row: Option<(String, i64)> = sqlx::query_as("SELECT type, time FROM tracker WHERE id = ?")
        .bind(entity_id)
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the tracker entry slot")?;
    if let Some((tracker_type, time)) = row
        && let Some(slot) = slots.slot(&tracker_type, time)
    {
        touched.insert((tracker_type, slot));
    }
    Ok(())
}

/// Keep one entry per tracker slot: the last-write-wins winner of the rows that
/// landed in it (`@@SYNC.md` §4.1).
///
/// Two devices logging the same non-cumulative slot while offline produce
/// several rows once they sync. Every device runs this cleanup over the same
/// events, so it is idempotent and emits no cleanup event of its own.
async fn dedup_tracker_slots(
    conn: &mut SqliteConnection,
    touched: &HashSet<(String, (i64, i64))>,
) -> Result<()> {
    for (tracker_type, (start, end)) in touched {
        let rows: Vec<Id> =
            sqlx::query_scalar("SELECT id FROM tracker WHERE type = ? AND time >= ? AND time < ?")
                .bind(tracker_type)
                .bind(start)
                .bind(end)
                .fetch_all(&mut *conn)
                .await
                .with_context(|| {
                    format!("Failed to read the '{tracker_type}' entries in slot {start}..{end}")
                })?;
        if rows.len() < 2 {
            continue;
        }
        // A row without a stamp cannot be ranked: leave the slot alone.
        let mut ranked: Vec<(Id, Stamp)> = Vec::with_capacity(rows.len());
        for id in &rows {
            match state::watermark(&mut *conn, *id, state::ENTITY).await? {
                Some(stamp) => ranked.push((*id, stamp)),
                None => {
                    ranked.clear();
                    break;
                }
            }
        }
        let mut ranked = ranked.into_iter();
        let Some((mut winner, mut best)) = ranked.next() else {
            continue;
        };
        for (id, stamp) in ranked {
            if state::wins(&stamp, &best) {
                winner = id;
                best = stamp;
            }
        }
        for id in &rows {
            if *id != winner {
                sqlx::query("DELETE FROM tracker WHERE id = ?")
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .context("Failed to drop a superseded tracker entry")?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

/// The outbox rows to push, oldest first.
pub async fn pending_events(pool: &SqlitePool) -> Result<Vec<super::types::SyncEvent>> {
    let rows = sqlx::query(
        "SELECT event_id, device_id, entity_id, timestamp, payload FROM _sync_events
         WHERE synced = 0 ORDER BY version ASC",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read the sync outbox")?;
    rows.iter()
        .map(|row| {
            let payload: Option<EntityPayload> =
                serde_json::from_str(row.get("payload")).context("Corrupt outbox payload")?;
            Ok(super::types::SyncEvent {
                event_id: row.get("event_id"),
                entity_id: row.get("entity_id"),
                device_id: row.get("device_id"),
                timestamp: row.get("timestamp"),
                payload,
            })
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

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn set_value(values: &mut Map<String, Value>, change: &Change) {
    let value = change.value.clone().unwrap_or(Value::Null);
    values.insert(change.field.to_string(), value);
}

fn text(values: &Map<String, Value>, field: &str) -> String {
    values
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn integer(values: &Map<String, Value>, field: &str) -> Option<i64> {
    values.get(field).and_then(Value::as_i64)
}

fn number(values: &Map<String, Value>, field: &str) -> Option<f64> {
    values.get(field).and_then(Value::as_f64)
}

fn flag(values: &Map<String, Value>, field: &str) -> bool {
    values.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn id_of(values: &Map<String, Value>, field: &str) -> Option<Id> {
    values.get(field).and_then(as_id)
}

fn as_id(value: &Value) -> Option<Id> {
    value.as_str().and_then(|text| Id::parse(text).ok())
}

fn json_opt<T: serde::Serialize>(value: Option<T>) -> Value {
    value
        .map(|value| serde_json::to_value(value).expect("a field value serializes"))
        .unwrap_or(Value::Null)
}

fn json_id(value: Option<Id>) -> Value {
    json_opt(value)
}

fn text_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn completion_todo_id(conflict: &Conflict) -> Result<Id> {
    match &conflict.event.event.payload {
        Some(EntityPayload::Completion(data)) => Ok(data.todo_id),
        _ => anyhow::bail!("a completion conflict must carry a completion payload"),
    }
}

fn conflict_kind_of(conflict: &Conflict) -> String {
    match &conflict.event.event.payload {
        Some(payload) => payload.kind().to_string(),
        None => "unknown".to_string(),
    }
}
