//! Engine tests: event emission, replay, LWW ordering and the promptable
//! delete conflicts.

use sqlx::SqlitePool;

use crate::config::{Config, TrackerInterval, TrackerKind, TrackerSetting};
use crate::db::{
    EventId, Id, TaskObject, create_entry, create_task, delete_task, test_pool, update_task,
};
use crate::sync::apply::{ApplyOutcome, ConflictKind, Resolution};
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
    crate::sync::EntityPayload::Task(crate::sync::TaskData {
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
        Some(crate::sync::EntityPayload::Task(_))
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
    assert!(matches!(outcome, ApplyOutcome::Applied));

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
            matches!(
                crate::sync::apply::apply_event(&pool, &older)
                    .await
                    .unwrap(),
                ApplyOutcome::Stale
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

#[tokio::test]
async fn local_edit_conflicts_with_remote_delete() {
    // Confirming the deletion drops the row and settles the local outbox.
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("local edit"))
        .await
        .unwrap();
    let delete = remote(Id::new(), future_ts(), id, None);
    let outcome = crate::sync::apply::apply_event(&pool, &delete)
        .await
        .unwrap();
    let ApplyOutcome::Conflict(conflict) = outcome else {
        panic!("a remote delete must not discard a local edit silently");
    };
    assert_eq!(conflict.kind, ConflictKind::RemoteDeleteVsLocalEdit);
    assert!(conflict.resurrectable);

    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::ConfirmRemote)
        .await
        .unwrap();
    assert_eq!(task_count(&pool, id).await, 0);
    assert!(
        crate::sync::apply::pending_events(&pool)
            .await
            .unwrap()
            .is_empty(),
        "the superseded local edit must leave the outbox"
    );

    // Resurrecting re-publishes the local state with a fresh timestamp.
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("keep me")).await.unwrap();
    let delete = remote(Id::new(), future_ts(), id, None);
    let outcome = crate::sync::apply::apply_event(&pool, &delete)
        .await
        .unwrap();
    let ApplyOutcome::Conflict(conflict) = outcome else {
        panic!("expected a conflict");
    };
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::Resurrect)
        .await
        .unwrap();
    assert_eq!(task_name(&pool, id).await.as_deref(), Some("keep me"));
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    let republished = pending.last().unwrap();
    assert!(republished.timestamp > delete.event.timestamp);
    assert!(matches!(
        republished.payload,
        Some(crate::sync::EntityPayload::Task(_))
    ));
}

#[tokio::test]
async fn remote_upsert_conflicts_with_local_delete() {
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("doomed")).await.unwrap();
    delete_task(&pool, id).await.unwrap();

    let upsert = remote(
        Id::new(),
        future_ts(),
        id,
        Some(task_payload("resurrect me")),
    );
    let outcome = crate::sync::apply::apply_event(&pool, &upsert)
        .await
        .unwrap();
    let ApplyOutcome::Conflict(conflict) = outcome else {
        panic!("a remote upsert must not silently undo a local delete");
    };
    assert_eq!(conflict.kind, ConflictKind::RemoteUpsertVsLocalDelete);

    // [1] keep the deletion: a fresh delete event settles it everywhere.
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::ConfirmRemote)
        .await
        .unwrap();
    assert_eq!(task_count(&pool, id).await, 0);
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert!(pending.last().unwrap().payload.is_none());

    // [2] keep the edit: the incoming snapshot is applied and re-published.
    let pool = test_pool().await.unwrap();
    let (id, _) = create_task(&pool, &task_object("doomed")).await.unwrap();
    delete_task(&pool, id).await.unwrap();
    let upsert = remote(
        Id::new(),
        future_ts(),
        id,
        Some(task_payload("resurrect me")),
    );
    let outcome = crate::sync::apply::apply_event(&pool, &upsert)
        .await
        .unwrap();
    let ApplyOutcome::Conflict(conflict) = outcome else {
        panic!("expected a conflict");
    };
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::Resurrect)
        .await
        .unwrap();
    assert_eq!(task_name(&pool, id).await.as_deref(), Some("resurrect me"));
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert!(matches!(
        pending.last().unwrap().payload,
        Some(crate::sync::EntityPayload::Task(_))
    ));
}

#[tokio::test]
async fn completion_on_a_deleted_task_can_resurrect_it() {
    let pool = test_pool().await.unwrap();
    let task = Id::new();
    let completion = Id::new();
    let other = Id::new();

    // The task lived here, then was deleted elsewhere: the watermark keeps
    // the snapshot that resurrection restores.
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

    let event = remote(
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
    );
    let outcome = crate::sync::apply::apply_event(&pool, &event)
        .await
        .unwrap();
    let ApplyOutcome::Conflict(conflict) = outcome else {
        panic!("a completion for a missing task must ask the user");
    };
    assert_eq!(conflict.kind, ConflictKind::CompletionOnMissingTask);
    assert!(conflict.resurrectable);

    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::Resurrect)
        .await
        .unwrap();
    assert_eq!(task_count(&pool, task).await, 1, "the task is restored");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(task)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the completion is kept");

    // Dropping it records an explicit delete instead.
    let pool = test_pool().await.unwrap();
    let drop_task = Id::new();
    let drop_completion = Id::new();
    crate::sync::apply::apply_event(
        &pool,
        &remote(other, 1_000, drop_task, Some(task_payload("gone"))),
    )
    .await
    .unwrap();
    crate::sync::apply::apply_event(&pool, &remote(other, 2_000, drop_task, None))
        .await
        .unwrap();
    let event = remote(
        other,
        3_000,
        drop_completion,
        Some(crate::sync::EntityPayload::Completion(
            crate::sync::CompletionData {
                todo_id: drop_task,
                time: 1_700_000_100,
                count: 1,
            },
        )),
    );
    let ApplyOutcome::Conflict(conflict) = crate::sync::apply::apply_event(&pool, &event)
        .await
        .unwrap()
    else {
        panic!("expected a conflict");
    };
    crate::sync::apply::resolve_conflict(&pool, &conflict, Resolution::ConfirmRemote)
        .await
        .unwrap();
    let pending = crate::sync::apply::pending_events(&pool).await.unwrap();
    assert!(pending.last().unwrap().payload.is_none());
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
        Some(crate::sync::EntityPayload::Mood(_))
    ));
    assert!(matches!(
        payloads[1],
        Some(crate::sync::EntityPayload::Tracker(_))
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
/// applies: the replay orders task upserts first (§4.5), so the foreign key
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
/// winner after the replay, and the cleanup emits nothing (§4.3): every
/// device derives the same state from the same events.
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
    let queued = crate::sync::apply::pending_events(&pool)
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
            Some(EntityPayload::Tracker(crate::sync::TrackerData {
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
            Some(EntityPayload::Tracker(crate::sync::TrackerData {
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
    // The remote entry's own tracker row is untouched by the cleanup.
    assert_eq!(
        crate::sync::apply::pending_events(&pool)
            .await
            .unwrap()
            .len(),
        queued,
        "the slot cleanup emits no event"
    );
}
