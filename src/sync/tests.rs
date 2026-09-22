//! Engine tests: event emission, replay, LWW ordering and the promptable
//! delete conflicts.

use sqlx::SqlitePool;

use crate::config::{Config, TrackerInterval, TrackerKind, TrackerSetting};
use crate::db::{
    EventId, Id, TaskObject, create_entry, create_task, delete_task, test_pool, update_task,
};
use crate::sync::apply::{ConflictKind, Resolution};
use crate::sync::{EntityPayload, RemoteEvent, state};
use crate::tracker::TrackerSlots;

fn task_object(name: &str) -> TaskObject {
    TaskObject {
        id: None,
        short_id: None,
        name: name.to_string(),
        body: String::new(),
        priority: 5,
        start_time: Some(1_700_000_000),
        available_duration_secs: None,
        interval_secs: None,
        target_count: 0,
        optional: false,
        end_time: None,
        parent: None,
    }
}

fn task_payload(name: &str) -> crate::sync::EntityPayload {
    crate::sync::EntityPayload::TaskCreate(crate::sync::TaskCreateData {
        name: name.to_string(),
        body: String::new(),
        priority: 5,
        start_time: Some(1_700_000_000),
        available_duration_secs: None,
        interval_secs: None,
        target_count: 0,
        optional: false,
        end_time: None,
        parent_id: None,
    })
}

fn remote(
    device: Id,
    timestamp: i64,
    entity_id: Id,
    payload: Option<crate::sync::EntityPayload>,
) -> RemoteEvent {
    RemoteEvent {
        version: 1,
        event: crate::sync::SyncEvent {
            event_id: EventId::new(),
            entity_id,
            device_id: device,
            timestamp,
            payload,
        },
    }
}

/// A timestamp that beats anything this device authored locally.
/// Whether applying left the event unapplied and uncontested.
fn is_stale(step: &crate::sync::apply::Applied) -> bool {
    !step.wrote && !step.discarded && step.conflicts.is_empty()
}

fn future_ts() -> i64 {
    state::now_ms() + 1_000_000
}

async fn task_name(pool: &SqlitePool, id: Id) -> Option<String> {
    sqlx::query_scalar("SELECT name FROM todos WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn task_count(pool: &SqlitePool, id: Id) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn mutations_append_outbox_events() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("write it")).await.unwrap();

    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert_eq!(pending.len(), 1, "the insert must queue one event");
    let event = &pending[0];
    assert_eq!(event.entity_id, id);
    assert!(matches!(
        event.payload,
        Some(crate::sync::EntityPayload::TaskCreate(_))
    ));

    update_task(&pool, id, 2).await.unwrap();
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert_eq!(pending.len(), 2, "the completion must queue its own event");
    assert!(matches!(
        pending[1].payload,
        Some(crate::sync::EntityPayload::Completion(_))
    ));

    // Consuming the completion again emits a delete for that row.
    update_task(&pool, id, -2).await.unwrap();
    let payloads: Vec<Option<crate::sync::EntityPayload>> =
        crate::sync::apply::pending_events(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect();
    assert_eq!(payloads.len(), 3);
    assert!(payloads[2].is_none(), "the consumed row must be deleted");

    delete_task(&pool, id).await.unwrap();
    let payloads: Vec<Option<crate::sync::EntityPayload>> =
        crate::sync::apply::pending_events(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect();
    assert!(payloads.last().unwrap().is_none(), "delete must be queued");
}

#[tokio::test]
async fn short_ids_are_local_only() {
    let pool = test_pool().await.unwrap();
    let (id, short_id) = create_task(&pool, &task_object("local")).await.unwrap();
    assert_eq!(short_id, 1);

    let payload = crate::sync::apply::pending_events(&pool).await.unwrap()[0]
        .payload
        .clone()
        .unwrap();
    let json = serde_json::to_string(&payload).unwrap();
    assert!(
        !json.contains("short_id"),
        "the projection must not reach the wire: {json}"
    );
    let _ = id;
}

#[tokio::test]
async fn remote_task_upsert_allocates_a_short_id() {
    let pool = test_pool().await.unwrap();
    let id = Id::new();
    let outcome = crate::sync::apply::apply_event(
        &pool,
        &remote(
            Id::new(),
            1_000,
            id,
            Some(task_payload("from the other device")),
        ),
    )
    .await
    .unwrap();
    assert!(outcome.wrote);

    let short_id: Option<i64> = sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(short_id, Some(1), "the projection is allocated on arrival");
}

#[tokio::test]
async fn lww_converges_regardless_of_replay_order() {
    let device_a = Id::new();
    let device_b = Id::new();
    let id = Id::new();

    for order in [0, 1] {
        let pool = test_pool().await.unwrap();
        let older = remote(device_a, 1_000, id, Some(task_payload("a")));
        let newer = remote(device_b, 2_000, id, Some(task_payload("b")));
        let events = if order == 0 {
            vec![older.clone(), newer.clone()]
        } else {
            vec![newer.clone(), older.clone()]
        };
        for event in &events {
            crate::sync::apply::apply_event(&pool, event).await.unwrap();
        }
        assert_eq!(
            task_name(&pool, id).await.as_deref(),
            Some("b"),
            "the newer (timestamp, device) must win in either order"
        );
        assert!(
            is_stale(
                &crate::sync::apply::apply_event(&pool, &older)
                    .await
                    .unwrap()
            ),
            "replaying the loser is a no-op"
        );
    }
}

#[tokio::test]
async fn remote_delete_removes_the_row() {
    let pool = test_pool().await.unwrap();
    let id = Id::new();
    crate::sync::apply::apply_event(
        &pool,
        &remote(Id::new(), 1_000, id, Some(task_payload("doomed"))),
    )
    .await
    .unwrap();
    crate::sync::apply::apply_event(&pool, &remote(Id::new(), 2_000, id, None))
        .await
        .unwrap();
    assert_eq!(task_count(&pool, id).await, 0);
}

/// A deletion is terminal: it is applied without asking, and nothing that
/// arrives for the entity afterwards is applied (`@@SYNC.md` §4.1.5).
#[tokio::test]
async fn a_deletion_is_terminal() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("doomed")).await.unwrap();
    let step = crate::sync::apply::apply_event(&pool, &remote(Id::new(), future_ts(), id, None))
        .await
        .unwrap();
    assert!(step.wrote, "the deletion is applied");
    assert!(step.conflicts.is_empty(), "a deletion asks nothing");
    assert_eq!(task_count(&pool, id).await, 0);

    // A newer edit and a whole creation snapshot arrive too late, however new
    // their timestamps are: the entity stays gone.
    for payload in [priority_change(9), task_payload("resurrect me")] {
        let step = crate::sync::apply::apply_event(
            &pool,
            &remote(Id::new(), future_ts() + 1_000, id, Some(payload)),
        )
        .await
        .unwrap();
        assert!(
            step.discarded,
            "a deleted entity discards what arrives later"
        );
        assert!(step.conflicts.is_empty());
    }
    assert_eq!(task_count(&pool, id).await, 0);
}

/// A deletion this device made holds against a remote edit, whatever the
/// timestamps say.
#[tokio::test]
async fn a_local_delete_is_not_undone_by_a_remote_edit() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("doomed")).await.unwrap();
    delete_task(&pool, id).await.unwrap();

    let step = crate::sync::apply::apply_event(
        &pool,
        &remote(
            Id::new(),
            future_ts() + 1_000,
            id,
            Some(task_payload("resurrect me")),
        ),
    )
    .await
    .unwrap();
    assert!(step.discarded, "the later edit still loses to the deletion");
    assert!(step.conflicts.is_empty(), "a deletion asks nothing");
    assert_eq!(task_count(&pool, id).await, 0);
    assert_eq!(task_name(&pool, id).await, None);
}

/// A completion for a task that was deleted elsewhere is dropped with it.
#[tokio::test]
async fn a_completion_for_a_deleted_task_is_dropped() {
    let pool = test_pool().await.unwrap();
    let other = Id::new();
    let task = Id::new();
    crate::sync::apply::apply_event(
        &pool,
        &remote(other, 1_000, task, Some(task_payload("habit"))),
    )
    .await
    .unwrap();
    crate::sync::apply::apply_event(&pool, &remote(other, 2_000, task, None))
        .await
        .unwrap();
    assert_eq!(task_count(&pool, task).await, 0);

    let completion = Id::new();
    let step = crate::sync::apply::apply_event(
        &pool,
        &remote(
            other,
            3_000,
            completion,
            Some(crate::sync::EntityPayload::Completion(
                crate::sync::CompletionData {
                    todo_id: task,
                    time: 1_700_000_100,
                    count: 1,
                },
            )),
        ),
    )
    .await
    .unwrap();
    assert!(step.discarded, "the completion goes with its task");
    assert!(step.conflicts.is_empty(), "a deletion asks nothing");
    assert_eq!(task_count(&pool, task).await, 0, "the task stays deleted");
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(task)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 0, "the completion is dropped, not resurrected");
}

#[tokio::test]
async fn concurrent_completions_from_two_devices_add_up() {
    let pool = test_pool().await.unwrap();
    let (task, _) = create_task(&pool, &task_object("pushups")).await.unwrap();
    for (count, timestamp) in [(2, 1_000), (3, 2_000)] {
        let event = remote(
            Id::new(),
            timestamp,
            Id::new(),
            Some(crate::sync::EntityPayload::Completion(
                crate::sync::CompletionData {
                    todo_id: task,
                    time: 1_700_000_000 + timestamp,
                    count,
                },
            )),
        );
        crate::sync::apply::apply_event(&pool, &event)
            .await
            .unwrap();
    }
    let total: i32 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
    )
    .bind(task)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 5, "distinct completion rows append");
}

#[tokio::test]
async fn mood_and_tracker_mutations_emit_events() {
    use crate::db::{EntryObject, TrackerObject, TrackerValue};

    let pool = test_pool().await.unwrap();
    let mood_id = create_entry(
        &pool,
        &EntryObject {
            mood: "good".to_string(),
            body: String::new(),
            time: 1_700_000_000,
            embedding: None,
            score: None,
            trackers: vec![TrackerObject {
                tracker_type: "sleep".to_string(),
                value: TrackerValue::Integer(7),
                replace_slot: None,
            }],
            duration: None,
            todo_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();

    let payloads: Vec<Option<crate::sync::EntityPayload>> =
        crate::sync::apply::pending_events(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect();
    assert_eq!(
        payloads.len(),
        2,
        "one event for the mood, one for the tracker"
    );
    assert!(matches!(
        payloads[0],
        Some(crate::sync::EntityPayload::MoodCreate(_))
    ));
    assert!(matches!(
        payloads[1],
        Some(crate::sync::EntityPayload::TrackerCreate(_))
    ));

    // Applying the mood elsewhere recreates the row without its local
    // embedding cache.
    let other = test_pool().await.unwrap();
    let mood_payload = payloads[0].clone().unwrap();
    crate::sync::apply::apply_event(
        &other,
        &remote(Id::new(), 1_000, mood_id, Some(mood_payload)),
    )
    .await
    .unwrap();
    let mood: String = sqlx::query_scalar("SELECT mood FROM mood WHERE id = ?")
        .bind(mood_id)
        .fetch_one(&other)
        .await
        .unwrap();
    assert_eq!(mood, "good");
}

/// Replacement-slot rules for one non-cumulative tracker type, as the CLI
/// builds them from the config.
fn slots_for(tracker_type: &str, slot_secs: i64) -> TrackerSlots {
    let mut config = Config::default();
    config.tracker.insert(
        tracker_type.to_string(),
        TrackerSetting::new(TrackerKind::Integer).with_interval(TrackerInterval {
            anchor: 0,
            span: jiff::Span::new().seconds(slot_secs),
            cumulative: false,
        }),
    );
    TrackerSlots::from_config(&config)
}

fn event(
    device: Id,
    version: i64,
    timestamp: i64,
    entity_id: Id,
    payload: Option<EntityPayload>,
) -> RemoteEvent {
    RemoteEvent {
        version,
        event: crate::sync::SyncEvent {
            event_id: EventId::new(),
            entity_id,
            device_id: device,
            timestamp,
            payload,
        },
    }
}

async fn cursor(pool: &SqlitePool) -> Option<String> {
    crate::sync::session::state_get(pool, crate::sync::KEY_LAST_SERVER_VERSION)
        .await
        .unwrap()
}

/// A page whose completion is logged before the task it references still
/// applies: the replay orders task upserts first (§4.3), so the foreign key
/// never fails and no conflict is raised. The cursor moves with the page.
#[tokio::test]
async fn a_page_applies_dependencies_before_dependents() {
    let pool = test_pool().await.unwrap();
    let task = Id::new();
    let completion = Id::new();
    let device = Id::new();
    let events = vec![
        event(
            device,
            1,
            2_000,
            completion,
            Some(EntityPayload::Completion(crate::sync::CompletionData {
                todo_id: task,
                time: 1_700_000_000,
                count: 1,
            })),
        ),
        event(device, 2, 1_000, task, Some(task_payload("parent"))),
    ];

    let outcome = crate::sync::apply::apply_page(&pool, &events, 2, &TrackerSlots::default())
        .await
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "replay order resolves the FK");
    assert_eq!(outcome.applied, 2);
    assert_eq!(task_count(&pool, task).await, 1);
    let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(task)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(logged, 1, "the completion lands once its task exists");
    assert_eq!(cursor(&pool).await.as_deref(), Some("2"));
}

/// Two devices that logged the same tracker slot while offline keep only the
/// winner after the replay: the loser is deleted locally and published as a
/// deletion of its own, so every device converges on the winner without
/// sharing this device's slots (`@@SYNC.md` §4.1.4).
#[tokio::test]
async fn a_replayed_tracker_slot_keeps_only_the_winner() {
    use crate::db::{EntryObject, TrackerObject, TrackerValue};

    let pool = test_pool().await.unwrap();
    let slots = slots_for("sleep", 3_600);
    let time = 1_700_000_000;
    create_entry(
        &pool,
        &EntryObject {
            mood: String::new(),
            body: String::new(),
            time,
            embedding: None,
            score: None,
            trackers: vec![TrackerObject {
                tracker_type: "sleep".to_string(),
                value: TrackerValue::Integer(4),
                replace_slot: Some((time, time + 3_600)),
            }],
            duration: None,
            todo_id: None,
        },
    )
    .await
    .unwrap();
    let local: Id = sqlx::query_scalar("SELECT id FROM tracker WHERE type = 'sleep'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let queued_before = crate::sync::apply::pending_events(&pool)
        .await
        .unwrap()
        .len();

    // The other device logged the same slot later: it wins the slot.
    let loser = Id::new();
    let winner = Id::new();
    let device = Id::new();
    let events = vec![
        event(
            device,
            1,
            state::now_ms() + 10_000,
            winner,
            Some(EntityPayload::TrackerCreate(crate::sync::TrackerData {
                tracker_type: "sleep".to_string(),
                score: crate::sync::TrackerScore::Integer(9),
                time,
                mood_id: None,
            })),
        ),
        event(
            device,
            2,
            state::now_ms() + 20_000,
            loser,
            Some(EntityPayload::TrackerCreate(crate::sync::TrackerData {
                tracker_type: "other".to_string(),
                score: crate::sync::TrackerScore::Integer(1),
                time,
                mood_id: None,
            })),
        ),
    ];

    let outcome = crate::sync::apply::apply_page(&pool, &events, 2, &slots)
        .await
        .unwrap();
    assert_eq!(outcome.applied, 2, "both remote entries land");

    // One sleep entry survives: the remote winner, not the local one.
    let entries: Vec<(Id, i64)> = sqlx::query_as(
        "SELECT id, CAST(score AS INTEGER) FROM tracker WHERE type = 'sleep' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(entries.len(), 1, "the slot keeps a single entry");
    assert_eq!(entries[0].0, winner, "the later log wins the slot");
    assert_eq!(entries[0].1, 9);
    // The superseded local entry is published as a tombstone of its own, so a
    // peer converges on the winner without knowing this device's slots (§4.1.4).
    let queued = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert_eq!(
        queued.len(),
        queued_before + 1,
        "the cleanup queues exactly one deletion"
    );
    let tombstone = queued.last().unwrap();
    assert!(tombstone.payload.is_none(), "the loser is deleted outright");
    assert_eq!(tombstone.entity_id, local, "the deleted row is the loser");
}

// ---------------------------------------------------------------------------
// Field-level diffs (§3, §4.1)
// ---------------------------------------------------------------------------

fn task_update(field: crate::sync::TaskUpdate) -> crate::sync::EntityPayload {
    crate::sync::EntityPayload::TaskUpdate(field)
}

fn priority_change(priority: i32) -> crate::sync::EntityPayload {
    task_update(crate::sync::TaskUpdate::Priority(priority))
}

async fn task_i64(pool: &SqlitePool, id: Id, column: &str) -> Option<i64> {
    let sql = format!("SELECT {column} FROM todos WHERE id = ?");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn task_body(pool: &SqlitePool, id: Id) -> Option<String> {
    sqlx::query_scalar("SELECT body FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn task_parent(pool: &SqlitePool, id: Id) -> Option<Id> {
    sqlx::query_scalar("SELECT parent FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A clear is an explicit enum variant with None and serializes to null.
#[test]
fn a_clear_is_an_explicit_variant_with_none() {
    let update = crate::sync::TaskUpdate::AvailableDurationSecs(None);
    let json = serde_json::to_string(&update).unwrap();
    assert_eq!(json, "{\"field\":\"available_duration_secs\",\"value\":null}");

    let parsed: crate::sync::TaskUpdate = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, update);

    let parent_clear = crate::sync::TaskUpdate::ParentId(None);
    let json_parent = serde_json::to_string(&parent_clear).unwrap();
    assert_eq!(json_parent, "{\"field\":\"parent_id\",\"value\":null}");
    let parsed_parent: crate::sync::TaskUpdate = serde_json::from_str(&json_parent).unwrap();
    assert_eq!(parsed_parent, parent_clear);

    let set_val = crate::sync::TaskUpdate::Priority(3);
    let json_set = serde_json::to_string(&set_val).unwrap();
    assert_eq!(json_set, "{\"field\":\"priority\",\"value\":3}");
    let parsed_set: crate::sync::TaskUpdate = serde_json::from_str(&json_set).unwrap();
    assert_eq!(parsed_set, set_val);
}

/// Two devices editing *different* fields keep both edits, whatever order the
/// events replay in (§4.1.1) — the property a per-entity watermark loses.
#[tokio::test]
async fn disjoint_field_updates_both_survive() {
    for reversed in [false, true] {
        let pool = test_pool().await.unwrap();
        let (id, _) = create_task(&pool, &task_object("shared")).await.unwrap();
        let older = remote(Id::new(), future_ts(), id, Some(priority_change(9)));
        let newer = remote(
            Id::new(),
            future_ts() + 500,
            id,
            Some(task_update(crate::sync::TaskUpdate::StartTime(Some(
                1_800_000_000,
            )))),
        );
        let events = if reversed {
            vec![newer, older]
        } else {
            vec![older, newer]
        };

        let page = crate::sync::apply::apply_page(&pool, &events, 2, &TrackerSlots::default())
            .await
            .unwrap();
        assert_eq!(page.applied, 2, "both fields are new information");
        assert_eq!(
            task_i64(&pool, id, "priority").await,
            Some(9),
            "the priority edit survives (reversed={reversed})"
        );
        assert_eq!(
            task_i64(&pool, id, "start_time").await,
            Some(1_800_000_000),
            "the start-time edit survives (reversed={reversed})"
        );
    }
}

/// The same field on two devices is last-write-wins, and replaying the loser
/// afterwards changes nothing (§4.1.2).
#[tokio::test]
async fn one_field_keeps_the_later_event() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("shared")).await.unwrap();
    let older = remote(Id::new(), future_ts(), id, Some(priority_change(3)));
    let newer = remote(Id::new(), future_ts() + 1, id, Some(priority_change(7)));

    crate::sync::apply::apply_event(&pool, &older)
        .await
        .unwrap();
    crate::sync::apply::apply_event(&pool, &newer)
        .await
        .unwrap();
    assert_eq!(task_i64(&pool, id, "priority").await, Some(7));

    assert!(
        is_stale(
            &crate::sync::apply::apply_event(&pool, &older)
                .await
                .unwrap()
        ),
        "the older value must not come back"
    );
    assert_eq!(task_i64(&pool, id, "priority").await, Some(7));
}

/// An edit that clears a nullable field propagates as a clear (§3).
#[tokio::test]
async fn a_cleared_field_reaches_the_other_device() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("shared")).await.unwrap();
    crate::sync::apply::apply_event(
        &pool,
        &remote(
            Id::new(),
            future_ts(),
            id,
            Some(task_update(crate::sync::TaskUpdate::AvailableDurationSecs(
                Some(600),
            ))),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        task_i64(&pool, id, "available_duration_secs").await,
        Some(600)
    );

    crate::sync::apply::apply_event(
        &pool,
        &remote(
            Id::new(),
            future_ts() + 1,
            id,
            Some(task_update(crate::sync::TaskUpdate::AvailableDurationSecs(
                None,
            ))),
        ),
    )
    .await
    .unwrap();
    assert_eq!(task_i64(&pool, id, "available_duration_secs").await, None);
}

// ---------------------------------------------------------------------------
// Text replacement conflicts (§4.2.1)
// ---------------------------------------------------------------------------

async fn note_conflict(pool: &SqlitePool) -> (Id, crate::sync::apply::Conflict) {
    let (id, _) = create_task(pool, &task_object("shared")).await.unwrap();
    crate::db::update_todo_body(pool, id, "local note")
        .await
        .unwrap();
    let incoming = remote(
        Id::new(),
        future_ts(),
        id,
        Some(task_update(crate::sync::TaskUpdate::Body(
            "remote note".to_string(),
        ))),
    );
    let step = crate::sync::apply::apply_event(pool, &incoming)
        .await
        .unwrap();
    assert!(!step.wrote, "a contested note is not written");
    let conflict = step
        .conflicts
        .into_iter()
        .next()
        .expect("both devices replaced the note");
    assert_eq!(conflict.kind, ConflictKind::TextReplaced);
    assert_eq!(conflict.field, Some(crate::sync::task_field::BODY));
    assert_eq!(conflict.local_value.as_deref(), Some("local note"));
    assert_eq!(conflict.remote_value.as_deref(), Some("remote note"));
    (id, conflict)
}

#[tokio::test]
async fn a_contested_note_can_take_the_incoming_text() {
    let pool = test_pool().await.unwrap();
    let (id, conflict) = note_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::KeepRemote)
        .await
        .unwrap();
    assert_eq!(task_body(&pool, id).await.as_deref(), Some("remote note"));
}

#[tokio::test]
async fn a_contested_note_can_keep_the_local_text() {
    let pool = test_pool().await.unwrap();
    let (id, conflict) = note_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::KeepLocal)
        .await
        .unwrap();
    assert_eq!(task_body(&pool, id).await.as_deref(), Some("local note"));

    // The choice is published, so it outranks the incoming text elsewhere.
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    let published = pending.last().unwrap();
    assert!(published.timestamp > conflict.event.event.timestamp);
    match &published.payload {
        Some(crate::sync::EntityPayload::TaskUpdate(crate::sync::TaskUpdate::Body(body))) => {
            assert_eq!(body, "local note");
        }
        other => panic!("expected a body update, got {other:?}"),
    }
}

#[tokio::test]
async fn a_contested_note_can_append_both() {
    let pool = test_pool().await.unwrap();
    let (id, conflict) = note_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::AppendBoth)
        .await
        .unwrap();
    assert_eq!(
        task_body(&pool, id).await.as_deref(),
        Some("local note\nremote note")
    );
}

// ---------------------------------------------------------------------------
// Parent cycles (§4.2.3)
// ---------------------------------------------------------------------------

async fn cycle_conflict(pool: &SqlitePool) -> (Id, Id, crate::sync::apply::Conflict) {
    let (a, _) = create_task(pool, &task_object("A")).await.unwrap();
    let (b, _) = create_task(pool, &task_object("B")).await.unwrap();
    // This device made A a child of B; the other device made B a child of A.
    crate::db::set_task_parent(pool, a, b).await.unwrap();
    let incoming = remote(
        Id::new(),
        future_ts(),
        b,
        Some(task_update(crate::sync::TaskUpdate::ParentId(Some(a)))),
    );
    let step = crate::sync::apply::apply_event(pool, &incoming)
        .await
        .unwrap();
    assert_eq!(
        task_parent(pool, b).await,
        None,
        "the cycle is not written before the user decides"
    );
    let conflict = step
        .conflicts
        .into_iter()
        .next()
        .expect("a cycle asks the user");
    assert_eq!(conflict.kind, ConflictKind::ParentCycle);
    (a, b, conflict)
}

#[tokio::test]
async fn a_parent_cycle_can_take_the_incoming_link() {
    let pool = test_pool().await.unwrap();
    let (a, b, conflict) = cycle_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::UseRemoteParent)
        .await
        .unwrap();
    assert_eq!(task_parent(&pool, b).await, Some(a));
    assert_eq!(task_parent(&pool, a).await, None, "the other link detached");
}

#[tokio::test]
async fn a_parent_cycle_can_keep_the_local_link() {
    let pool = test_pool().await.unwrap();
    let (a, b, conflict) = cycle_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::UseLocalParent)
        .await
        .unwrap();
    assert_eq!(task_parent(&pool, a).await, Some(b), "this device's link");
    assert_eq!(task_parent(&pool, b).await, None);
}

#[tokio::test]
async fn a_parent_cycle_can_detach_both() {
    let pool = test_pool().await.unwrap();
    let (a, b, conflict) = cycle_conflict(&pool).await;
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::DetachBoth)
        .await
        .unwrap();
    assert_eq!(task_parent(&pool, a).await, None);
    assert_eq!(task_parent(&pool, b).await, None);
}

/// A device that decided neither link repairs the cycle on its own: the later
/// link wins and the older one is detached, with no event and no prompt.
#[tokio::test]
async fn a_third_device_repairs_a_cycle_deterministically() {
    for reversed in [false, true] {
        let pool = test_pool().await.unwrap();
        let (a, _) = create_task(&pool, &task_object("A")).await.unwrap();
        let (b, _) = create_task(&pool, &task_object("B")).await.unwrap();
        let ab = remote(
            Id::new(),
            future_ts(),
            a,
            Some(task_update(crate::sync::TaskUpdate::ParentId(Some(b)))),
        );
        let ba = remote(
            Id::new(),
            future_ts() + 500,
            b,
            Some(task_update(crate::sync::TaskUpdate::ParentId(Some(a)))),
        );
        let events = if reversed { vec![ba, ab] } else { vec![ab, ba] };

        let page = crate::sync::apply::apply_page(&pool, &events, 2, &TrackerSlots::default())
            .await
            .unwrap();
        assert!(
            page.conflicts.is_empty(),
            "no local decision, no prompt (reversed={reversed})"
        );
        assert_eq!(
            task_parent(&pool, b).await,
            Some(a),
            "the later link is the parent (reversed={reversed})"
        );
        assert_eq!(
            task_parent(&pool, a).await,
            None,
            "the older link is detached (reversed={reversed})"
        );
    }
}
