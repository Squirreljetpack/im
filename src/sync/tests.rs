//! Unit tests of the client half: what a mutation queues, and what a served
//! page does to the domain tables.

use sqlx::{Row, SqlitePool};

use super::apply;
use super::types::{CellEvent, CellValue, Cursor, EXISTENCE, LogRow, live};
use crate::db::{TaskObject, create_task, delete_task, test_pool, update_todo_body};

/// A cursor names the log it indexes, and a position that names none is not a
/// position: the database is treated as one that has read nowhere yet.
#[test]
fn a_cursor_keeps_the_log_it_names() {
    let named = Cursor {
        log: "log-id".to_string(),
        version: 7,
    };
    assert_eq!(Cursor::parse(&named.encode()), Some(named));
    assert_eq!(Cursor::parse("412"), None);
    assert_eq!(Cursor::parse("/0"), None);
    assert_eq!(Cursor::parse("log-id/not-a-version"), None);
}

fn task(name: &str) -> TaskObject {
    TaskObject {
        id: None,
        short_id: None,
        name: name.to_string(),
        body: String::new(),
        priority: 5,
        start_time: 1_700_000_000,
        available_duration_secs: None,
        interval_secs: None,
        target_count: 0,
        optional: false,
        end_time: None,
        parent: None,
    }
}

/// The outbox read as if it were a served page: outbox order is delivery order.
async fn queued(pool: &SqlitePool) -> Vec<LogRow> {
    let rows = sqlx::query(
        "SELECT id, event_id, device_id, entity_id, kind, field, value, timestamp
           FROM _sync_events ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("read the outbox");
    rows.into_iter()
        .map(|row| {
            let value: Option<String> = row.get("value");
            LogRow {
                version: row.get("id"),
                cell: CellEvent {
                    event_id: row.get("event_id"),
                    device_id: row.get("device_id"),
                    entity_id: row.get("entity_id"),
                    kind: row.get("kind"),
                    field: row.get("field"),
                    value: value.map(|json| serde_json::from_str(&json).expect("a cell value")),
                    timestamp: row.get("timestamp"),
                },
            }
        })
        .collect()
}

#[tokio::test]
async fn a_create_queues_an_existence_cell_then_its_fields() {
    let pool = test_pool().await.expect("in-memory db");
    create_task(&pool, &task("write it")).await.expect("create");

    let rows = queued(&pool).await;
    assert_eq!(rows[0].cell.field, EXISTENCE);
    assert_eq!(rows[0].cell.value, Some(CellValue::text("live")));
    assert_eq!(rows[0].cell.kind, "task");

    let name = rows
        .iter()
        .find(|row| row.cell.field == "name")
        .expect("the name cell");
    assert_eq!(name.cell.value, Some(CellValue::text("write it")));
    // Null fields are not cells: the peer's insert falls back to the column
    // default, which is what this device did.
    assert!(!rows.iter().any(|row| row.cell.field == "end_time"));
}

#[tokio::test]
async fn an_edit_queues_one_cell() {
    let pool = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&pool, &task("body")).await.expect("create");
    sqlx::query("DELETE FROM _sync_events")
        .execute(&pool)
        .await
        .unwrap();

    update_todo_body(&pool, id, "new body").await.expect("edit");
    let rows = queued(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cell.field, "body");
    assert_eq!(rows[0].cell.value, Some(CellValue::text("new body")));
}

#[tokio::test]
async fn a_delete_drops_pending_field_edits_and_queues_its_marker() {
    let pool = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&pool, &task("doomed")).await.expect("create");
    update_todo_body(&pool, id, "edited before delete")
        .await
        .expect("edit");

    let cells = super::events::row_cells(&mut *pool.acquire().await.unwrap(), "task", id)
        .await
        .expect("snapshot")
        .expect("live row has a snapshot");
    assert!(
        cells
            .iter()
            .any(|(f, v)| f == "name" && *v == CellValue::text("doomed"))
    );
    assert!(
        cells
            .iter()
            .any(|(f, v)| f == "body" && *v == CellValue::text("edited before delete"))
    );

    delete_task(&pool, id).await.expect("delete");
    let rows = queued(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cell.field, EXISTENCE);
    assert_eq!(rows[0].cell.value, Some(CellValue::text("deleted")));
}

#[tokio::test]
async fn deleting_a_task_retires_cells_for_its_cascade() {
    let pool = test_pool().await.expect("in-memory db");
    let (task_id, _) = create_task(&pool, &task("parent")).await.expect("create");

    let mut child = task("child");
    child.parent = Some(task_id);
    let (child_id, _) = create_task(&pool, &child).await.expect("create");

    let mood_id = crate::db::create_entry(
        &pool,
        &crate::db::EntryObject {
            mood: "calm".to_string(),
            body: String::new(),
            time: crate::date::LogTime {
                epoch: 1_700_000_000,
                offset: 0,
            },
            location: "home".to_string(),
            embedding: None,
            score: None,
            trackers: Vec::new(),
            duration: None,
            todo_id: Some(task_id),
        },
    )
    .await
    .expect("create entry")
    .expect("mood id");

    crate::db::set_scheduled_completion(&pool, task_id, Some(1))
        .await
        .expect("complete");
    let completion_id: crate::db::Id =
        sqlx::query_scalar("SELECT id FROM todo_completions WHERE todo_id = ?")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .expect("completion row");

    delete_task(&pool, task_id).await.expect("delete");

    let rows = queued(&pool).await;
    assert!(
        !rows.iter().any(|row| row.cell.entity_id == completion_id),
        "the cascaded completion has no cells left"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.cell.entity_id == mood_id && row.cell.field == "todo_id"),
        "the cleared mood link is not pushed"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.cell.entity_id == child_id && row.cell.field == "parent_id"),
        "the cleared child link is not pushed"
    );
    assert!(
        rows.iter()
            .any(|row| row.cell.entity_id == mood_id && row.cell.field == EXISTENCE),
        "the mood itself survives"
    );
    assert!(
        rows.iter()
            .any(|row| row.cell.entity_id == child_id && row.cell.field == EXISTENCE),
        "the child itself survives"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.cell.entity_id == task_id)
            .count(),
        1,
        "the task is left with just its tombstone"
    );
}

#[tokio::test]
async fn repeated_edits_to_one_field_coalesce_in_the_outbox() {
    let pool = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&pool, &task("draft")).await.expect("create");
    sqlx::query("DELETE FROM _sync_events")
        .execute(&pool)
        .await
        .unwrap();

    update_todo_body(&pool, id, "first").await.expect("edit 1");
    let first = queued(&pool).await;
    assert_eq!(first.len(), 1);

    update_todo_body(&pool, id, "second").await.expect("edit 2");
    let second = queued(&pool).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].cell.field, "body");
    assert_eq!(second[0].cell.value, Some(CellValue::text("second")));
    assert_ne!(second[0].cell.event_id, first[0].cell.event_id);
    assert!(second[0].cell.timestamp > first[0].cell.timestamp);
}

#[tokio::test]
async fn a_newer_pending_local_edit_is_not_clobbered_by_an_older_pulled_cell() {
    let pool = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&pool, &task("shared")).await.expect("create");
    sqlx::query("DELETE FROM _sync_events")
        .execute(&pool)
        .await
        .unwrap();

    update_todo_body(&pool, id, "local newer").await.expect("local edit");
    let local = queued(&pool).await;

    let stale_remote = LogRow {
        version: 1,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: id,
            kind: "task".to_string(),
            field: "body".to_string(),
            value: Some(CellValue::text("remote older")),
            timestamp: local[0].cell.timestamp - 1000,
        },
    };
    apply::apply_rows(
        &mut pool.acquire().await.unwrap(),
        &[stale_remote],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply stale remote");

    let body: String = sqlx::query_scalar("SELECT body FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(body, "local newer");
}

#[tokio::test]
async fn a_served_completion_retires_a_oneshot_task_short_id() {
    let source = test_pool().await.expect("in-memory db");
    let (task_id, _) = create_task(&source, &task("oneshot")).await.expect("create");
    crate::db::set_scheduled_completion(&source, task_id, Some(1))
        .await
        .expect("complete");
    let page = queued(&source).await;

    let target = test_pool().await.expect("in-memory db");
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &page,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply task + completion");

    let short_id: Option<i64> = sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
        .bind(task_id)
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(short_id, None, "completed oneshot task retires its short_id");
}

#[tokio::test]
async fn a_served_create_materialises_a_row_with_its_local_projection() {
    let source = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&source, &task("replicated"))
        .await
        .expect("create");
    let page = queued(&source).await;

    let target = test_pool().await.expect("in-memory db");
    let applied = apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &page,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply");
    assert_eq!(applied.created, 1);
    assert!(applied.skipped.is_empty(), "{:?}", applied.skipped);

    let name: String = sqlx::query_scalar("SELECT name FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&target)
        .await
        .expect("the row");
    assert_eq!(name, "replicated");
    let short_id: Option<i64> = sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(short_id, Some(1), "the projection is allocated locally");
}

#[tokio::test]
async fn a_served_cell_updates_without_recreating() {
    let source = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&source, &task("before")).await.expect("create");
    let create = queued(&source).await;
    let target = test_pool().await.expect("in-memory db");
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &create,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply");

    sqlx::query("DELETE FROM _sync_events")
        .execute(&source)
        .await
        .unwrap();
    update_todo_body(&source, id, "after").await.expect("edit");
    let edit = queued(&source).await;
    let applied = apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &edit,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply the edit");
    assert_eq!(applied.updated, 1);
    let body: String = sqlx::query_scalar("SELECT body FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(body, "after");
}

#[tokio::test]
async fn a_served_delete_takes_the_dependants_with_it() {
    let source = test_pool().await.expect("in-memory db");
    let (parent, _) = create_task(&source, &task("parent")).await.expect("create");
    let mut child = task("child");
    child.parent = Some(parent);
    let (child_id, _) = create_task(&source, &child).await.expect("create");
    let create = queued(&source).await;

    let target = test_pool().await.expect("in-memory db");
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &create,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply the creates");

    delete_task(&source, parent).await.expect("delete");
    let delete: Vec<LogRow> = queued(&source)
        .await
        .into_iter()
        .filter(|row| row.cell.entity_id == parent && row.cell.field == EXISTENCE)
        .collect();
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &delete,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply the delete");

    let gone: Option<i64> = sqlx::query_scalar("SELECT 1 FROM todos WHERE id = ?")
        .bind(parent)
        .fetch_optional(&target)
        .await
        .unwrap();
    assert!(gone.is_none(), "the row is gone");
    let link: Option<String> = sqlx::query_scalar("SELECT parent FROM todos WHERE id = ?")
        .bind(child_id)
        .fetch_one(&target)
        .await
        .expect("the child survives");
    assert_eq!(link, None, "and is promoted to a root");
}

#[tokio::test]
async fn a_served_task_delete_retires_pending_dependants() {
    let pool = test_pool().await.expect("in-memory db");
    let (task_id, _) = create_task(&pool, &task("parent")).await.expect("create");
    crate::db::set_scheduled_completion(&pool, task_id, Some(1))
        .await
        .expect("complete");
    let completion_id: crate::db::Id =
        sqlx::query_scalar("SELECT id FROM todo_completions WHERE todo_id = ?")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .expect("completion row");

    let delete = LogRow {
        version: 99,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: task_id,
            kind: "task".to_string(),
            field: EXISTENCE.to_string(),
            value: super::types::deleted(),
            timestamp: 99,
        },
    };
    apply::apply_rows(
        &mut pool.acquire().await.unwrap(),
        &[delete],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply the delete");

    let rows = queued(&pool).await;
    assert!(
        !rows.iter().any(|row| row.cell.entity_id == completion_id),
        "a served task delete retires the completion it cascaded"
    );
}

#[tokio::test]
async fn a_cell_for_a_row_that_never_appeared_is_skipped() {
    let pool = test_pool().await.expect("in-memory db");
    let orphan = LogRow {
        version: 1,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: crate::db::Id::new(),
            kind: "task".to_string(),
            field: "name".to_string(),
            value: Some(CellValue::text("nameless")),
            timestamp: 1,
        },
    };
    let applied = apply::apply_rows(
        &mut pool.acquire().await.unwrap(),
        &[orphan],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply");
    assert_eq!(applied.updated, 0);
    assert_eq!(applied.skipped.len(), 1);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no skeleton row is invented");
}

/// A required cell's register row moves forward whenever it is rewritten, so a
/// create can straddle pages. The row must not be invented from what arrived
/// first, and the cursor must not pass what is still being built.
#[tokio::test]
async fn a_create_split_across_pages_waits_for_its_required_cells() {
    let pool = test_pool().await.expect("in-memory db");
    let id = crate::db::Id::new();
    let device = crate::db::Id::new();
    let cell = |version: i64, field: &str, value: Option<CellValue>| LogRow {
        version,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: device,
            entity_id: id,
            kind: "task".to_string(),
            field: field.to_string(),
            value,
            timestamp: version,
        },
    };
    let existence = cell(10, EXISTENCE, live());
    let name = cell(90, "name", Some(CellValue::text("late")));
    let start = cell(95, "start_time", Some(CellValue::Int(1_700_000_000)));

    let mut conn = pool.acquire().await.unwrap();
    let mut pending = apply::Pending::default();
    let first = apply::apply_rows(&mut conn, &[existence], &mut pending)
        .await
        .expect("apply the existence cell");
    assert_eq!(first.created, 0, "an existence cell alone is not a row");
    assert!(first.skipped.is_empty(), "and not a loss either");
    assert_eq!(
        pending.fence(50),
        9,
        "the cursor cannot pass the create it holds"
    );

    let second = apply::apply_rows(&mut conn, &[name], &mut pending)
        .await
        .expect("apply the name");
    assert_eq!(second.created, 0, "a task is still short its start time");
    assert_eq!(pending.fence(100), 9, "so the fence stays where it was held");

    let third = apply::apply_rows(&mut conn, &[start], &mut pending)
        .await
        .expect("apply the start time");
    assert_eq!(third.created, 1, "the last required cell completes the create");
    assert_eq!(pending.fence(95), 95, "and the fence may move on");

    let name: String = sqlx::query_scalar("SELECT name FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .expect("the row");
    assert_eq!(name, "late");
}

#[tokio::test]
async fn a_served_completion_update_and_delete_refresh_task_short_id() {
    let source = test_pool().await.expect("in-memory db");
    let (task_id, _) = create_task(&source, &task("oneshot")).await.expect("create");
    crate::db::set_scheduled_completion(&source, task_id, Some(1))
        .await
        .expect("complete");
    let create_page = queued(&source).await;
    let completion_id = create_page
        .iter()
        .find(|row| row.cell.kind == "completion")
        .expect("completion cell")
        .cell
        .entity_id;

    let target = test_pool().await.expect("in-memory db");
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &create_page,
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply task + completion");

    let short_after_done: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
            .bind(task_id)
            .fetch_one(&target)
            .await
            .unwrap();
    assert_eq!(short_after_done, None);

    // Updating completion.count to unmarked (-1) restores a short_id.
    let unmark = LogRow {
        version: 10,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: completion_id,
            kind: "completion".to_string(),
            field: "count".to_string(),
            value: Some(CellValue::Int(i64::from(crate::db::SCHEDULED_UNMARKED))),
            timestamp: 10,
        },
    };
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &[unmark],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply unmark");
    let short_after_unmark: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
            .bind(task_id)
            .fetch_one(&target)
            .await
            .unwrap();
    assert_eq!(short_after_unmark, Some(1));

    // Re-marking done retires short_id, and deleting the completion row restores it.
    let mark_done = LogRow {
        version: 11,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: completion_id,
            kind: "completion".to_string(),
            field: "count".to_string(),
            value: Some(CellValue::Int(1)),
            timestamp: 11,
        },
    };
    let delete_completion = LogRow {
        version: 12,
        cell: CellEvent {
            event_id: crate::db::EventId::new(),
            device_id: crate::db::Id::new(),
            entity_id: completion_id,
            kind: "completion".to_string(),
            field: EXISTENCE.to_string(),
            value: super::types::deleted(),
            timestamp: 12,
        },
    };
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &[mark_done],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply mark done");
    apply::apply_rows(
        &mut target.acquire().await.unwrap(),
        &[delete_completion],
        &mut apply::Pending::default(),
    )
    .await
    .expect("apply completion delete");
    let short_after_delete: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
            .bind(task_id)
            .fetch_one(&target)
            .await
            .unwrap();
    assert_eq!(short_after_delete, Some(1));
}

#[tokio::test]
async fn delete_mood_nulls_linked_tracker_mood_without_deleting_tracker() {
    let pool = test_pool().await.expect("in-memory db");
    let mood_id = crate::db::create_entry(
        &pool,
        &crate::db::EntryObject {
            mood: "calm".to_string(),
            body: "notes".to_string(),
            time: crate::date::LogTime {
                epoch: 1_700_000_000,
                offset: 0,
            },
            location: "home".to_string(),
            embedding: None,
            score: None,
            trackers: vec![crate::db::TrackerObject {
                tracker_type: "weight".to_string(),
                value: crate::db::TrackerValue::Integer(70),
                replace_slot: None,
            }],
            duration: None,
            todo_id: None,
        },
    )
    .await
    .expect("create entry")
    .expect("mood id");
    let tracker_id: crate::db::Id = sqlx::query_scalar("SELECT id FROM tracker WHERE mood = ?")
        .bind(mood_id)
        .fetch_one(&pool)
        .await
        .expect("linked tracker");
    sqlx::query("DELETE FROM _sync_events")
        .execute(&pool)
        .await
        .unwrap();

    crate::db::delete_mood(&pool, mood_id)
        .await
        .expect("delete mood");

    let linked_mood: Option<crate::db::Id> =
        sqlx::query_scalar("SELECT mood FROM tracker WHERE id = ?")
            .bind(tracker_id)
            .fetch_one(&pool)
            .await
            .expect("tracker row survives mood delete");
    assert_eq!(linked_mood, None);

    let rows = queued(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cell.entity_id, mood_id);
    assert_eq!(rows[0].cell.field, EXISTENCE);
    assert_eq!(rows[0].cell.value, Some(CellValue::text("deleted")));
}

#[tokio::test]
async fn resurrect_recreates_mood_and_drops_dangling_task_parent() {
    let pool = test_pool().await.expect("in-memory db");
    let mood_offer = super::session::Resurrect {
        entity_id: crate::db::Id::new(),
        kind: "mood".to_string(),
        cells: vec![
            ("mood".to_string(), CellValue::text("joyful")),
            ("body".to_string(), CellValue::text("sunny walk")),
            ("location".to_string(), CellValue::text("park")),
            ("time".to_string(), CellValue::Int(1_700_000_000)),
            ("tz_offset".to_string(), CellValue::Int(0)),
        ],
        device: Some("peer".to_string()),
        at: Some(1_700_000_000_000),
    };
    let copy_mood = super::session::resurrect(&pool, &mood_offer)
        .await
        .expect("resurrect mood")
        .expect("new mood id");
    let body: String = sqlx::query_scalar("SELECT body FROM mood WHERE id = ?")
        .bind(copy_mood)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(body, "sunny walk");

    let missing_parent = crate::db::Id::new();
    let task_offer = super::session::Resurrect {
        entity_id: crate::db::Id::new(),
        kind: "task".to_string(),
        cells: vec![
            ("name".to_string(), CellValue::text("orphan copy")),
            ("start_time".to_string(), CellValue::Int(1_700_000_000)),
            (
                "parent_id".to_string(),
                CellValue::text(missing_parent.to_string()),
            ),
        ],
        device: Some("peer".to_string()),
        at: Some(1_700_000_000_000),
    };
    let copy_task = super::session::resurrect(&pool, &task_offer)
        .await
        .expect("resurrect task")
        .expect("new task id");
    let parent: Option<String> = sqlx::query_scalar("SELECT parent FROM todos WHERE id = ?")
        .bind(copy_task)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(parent, None, "dangling parent link is dropped on resurrect");
}
