//! Unit tests of the client half: what a mutation queues, and what a served
//! page does to the domain tables.

use sqlx::{Row, SqlitePool};

use super::apply;
use super::types::{CellEvent, CellValue, EXISTENCE, LogRow, live};
use crate::db::{TaskObject, create_task, delete_task, test_pool, update_todo_body};

fn task(name: &str) -> TaskObject {
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
async fn a_delete_queues_its_marker_and_a_snapshot() {
    let pool = test_pool().await.expect("in-memory db");
    let (id, _) = create_task(&pool, &task("doomed")).await.expect("create");
    sqlx::query("DELETE FROM _sync_events")
        .execute(&pool)
        .await
        .unwrap();

    delete_task(&pool, id).await.expect("delete");
    let rows = queued(&pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cell.field, EXISTENCE);
    assert_eq!(rows[0].cell.value, Some(CellValue::text("deleted")));

    let image: Option<String> =
        sqlx::query_scalar("SELECT image FROM _sync_events WHERE event_id = ?")
            .bind(rows[0].cell.event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let image = image.expect("the delete carries a snapshot");
    assert!(image.contains("doomed"), "{image}");
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
        &mut *target.acquire().await.unwrap(),
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
        &mut *target.acquire().await.unwrap(),
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
        &mut *target.acquire().await.unwrap(),
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
        &mut *target.acquire().await.unwrap(),
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
        &mut *target.acquire().await.unwrap(),
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
        &mut *pool.acquire().await.unwrap(),
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
/// create can straddle two pages. The row must not be invented from what
/// arrived first, and the cursor must not pass what is still being built.
#[tokio::test]
async fn a_create_split_across_pages_waits_for_its_required_cell() {
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
    assert_eq!(second.created, 1, "the required cell completes the create");
    assert_eq!(pending.fence(90), 90, "and the fence may move on");

    let name: String = sqlx::query_scalar("SELECT name FROM todos WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .expect("the row");
    assert_eq!(name, "late");
}
