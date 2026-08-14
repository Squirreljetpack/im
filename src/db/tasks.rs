use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use crate::types::TaskKind;

use super::models::{PrunedTask, TaskObject, TaskRow, TaskUpdateInfo, UpdateTaskObject};

/// Insert a new task. Both the stable row id and the user-facing `short_id`
/// are assigned by the database layer — the caller must not pass either
/// (`task.id` and `task.short_id` must be `None`). Returns the row id and
/// the allocated short id.
pub async fn create_task(pool: &SqlitePool, task: &TaskObject) -> Result<(i64, i64)> {
    assert!(
        task.short_id.is_none(),
        "create_task assigns the short id itself; the task must not carry one"
    );
    assert!(
        task.id.is_none(),
        "create_task assigns the row id itself; the task must not carry one"
    );
    assert!(
        task.interval_secs.is_none_or(|i| i > 0),
        "interval_secs must be None or positive, got {:?}",
        task.interval_secs
    );
    let short_id = allocate_short_id(pool).await?;

    let row = sqlx::query(
        r#"INSERT INTO todos (name, body, priority, short_id, start_time, available_duration_secs, interval_secs, target_count, optional, end_time, parent)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id"#,
    )
    .bind(&task.name)
    .bind(&task.body)
    .bind(task.priority)
    .bind(short_id)
    .bind(task.start_time)
    .bind(task.available_duration_secs)
    .bind(task.interval_secs)
    .bind(task.target_count)
    .bind(if task.optional { 1 } else { 0 })
    .bind(task.end_time)
    .bind(task.parent)
    .fetch_one(pool)
    .await
    .context("Failed to create task")?;

    let id: i64 = row.get("id");
    Ok((id, short_id))
}

/// Update the recurring-task fields of an existing task. Returns the number
/// of affected rows.
pub async fn edit_task(pool: &SqlitePool, update: &UpdateTaskObject) -> Result<u64> {
    assert!(
        update.interval_secs.is_none_or(|i| i > 0),
        "interval_secs must be None or positive, got {:?}",
        update.interval_secs
    );
    let res = sqlx::query(
        r#"UPDATE todos SET interval_secs = ?, available_duration_secs = ?, target_count = ?,
                   optional = ?, end_time = ? WHERE id = ?"#,
    )
    .bind(update.interval_secs)
    .bind(update.available_duration_secs)
    .bind(update.target_count)
    .bind(if update.optional { 1 } else { 0 })
    .bind(update.end_time)
    .bind(update.id)
    .execute(pool)
    .await
    .context("Failed to update recurring task")?;
    Ok(res.rows_affected())
}

/// Delete a task row; `todo_completions` rows cascade via `ON DELETE CASCADE`.
pub async fn delete_task(pool: &SqlitePool, id: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM todos WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to delete task")?;
    Ok(res.rows_affected())
}

/// Apply a completion delta to a task and return the new total.
///
/// Positive deltas append a new completion event with the delta as its count;
/// negative deltas consume the most recent events within the current interval
/// (recurring tasks: entries from before the current interval started are
/// never touched, and the returned total is the sum within the current
/// interval only; oneshot tasks: full history).
///
/// After applying the delta the task's `short_id` is synced to its completion
/// state (see [`sync_short_id`]): a oneshot task that just completed loses
/// its short id; a oneshot task that just became not-done again is assigned
/// the smallest free one.
pub async fn update_task(pool: &SqlitePool, todo_id: i64, delta: i32) -> Result<i32> {
    // Determine the current interval boundary for recurring tasks so we never
    // touch completion events from before the current interval started.
    let interval_start: Option<i64> =
        sqlx::query("SELECT start_time, interval_secs FROM todos WHERE id = ?")
            .bind(todo_id)
            .fetch_optional(pool)
            .await?
            .and_then(|row| {
                let start: Option<i64> = row.get("start_time");
                let interval: Option<i64> = row.get("interval_secs");
                match (start, interval) {
                    (Some(st), Some(iv)) if iv > 0 => Some(crate::task::current_interval_start(
                        st,
                        crate::date::db_to_span(iv),
                        crate::date::now(),
                    )),
                    _ => None,
                }
            });

    if delta > 0 {
        sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, ?)")
            .bind(todo_id)
            .bind(crate::date::now())
            .bind(delta)
            .execute(pool)
            .await?;
    } else if delta < 0 {
        let rows = match interval_start {
            Some(boundary) => sqlx::query(
                "SELECT id, count FROM todo_completions WHERE todo_id = ? AND time >= ? ORDER BY id ASC",
            )
            .bind(todo_id)
            .bind(boundary)
            .fetch_all(pool)
            .await?,
            None => sqlx::query(
                "SELECT id, count FROM todo_completions WHERE todo_id = ? ORDER BY id ASC",
            )
            .bind(todo_id)
            .fetch_all(pool)
            .await?,
        };
        let ids: Vec<i64> = rows.iter().map(|r| r.get("id")).collect();
        let counts: Vec<i32> = rows.iter().map(|r| r.get("count")).collect();
        let new_counts = crate::task::apply_delta_to_counts(&counts, delta);
        // Trailing entries were fully consumed → delete them in a single batch query.
        let to_delete = &ids[new_counts.len()..];
        if !to_delete.is_empty() {
            let sql = format!(
                "DELETE FROM todo_completions WHERE id IN ({})",
                to_delete.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
            for id in to_delete {
                q = q.bind(id);
            }
            q.execute(pool).await?;
        }
        // The last surviving entry may have been partially reduced.
        if let Some(&nc) = new_counts.last() {
            let orig = counts[new_counts.len() - 1];
            if nc != orig {
                sqlx::query("UPDATE todo_completions SET count = ? WHERE id = ?")
                    .bind(nc)
                    .bind(ids[new_counts.len() - 1])
                    .execute(pool)
                    .await?;
            }
        }
    }
    // Return the new total: within the current interval for recurring tasks,
    // the full sum otherwise.
    let total: i32 = match interval_start {
        Some(boundary) => sqlx::query_scalar(
            "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ? AND time >= ?",
        )
        .bind(todo_id)
        .bind(boundary)
        .fetch_one(pool)
        .await?,
        None => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
            )
            .bind(todo_id)
            .fetch_one(pool)
            .await?
        }
    };
    sync_short_id(pool, todo_id).await?;
    Ok(total)
}

/// Delete completed oneshot tasks and expired (end_time passed) recurring
/// tasks in one `RETURNING` statement so the report reflects exactly the
/// rows deleted. A oneshot task counts as completed when its completion
/// state satisfies `is_task_done`.
pub async fn prune_tasks(pool: &SqlitePool, now: i64) -> Result<Vec<PrunedTask>> {
    let rows = sqlx::query(
        r#"DELETE FROM todos
           WHERE (interval_secs IS NULL AND (
                     (target_count <= 0 AND EXISTS(SELECT 1 FROM todo_completions tc
                                                    WHERE tc.todo_id = todos.id AND tc.count > 0))
                  OR (target_count > 0 AND COALESCE((SELECT SUM(count) FROM todo_completions tc
                                                      WHERE tc.todo_id = todos.id), 0) >= target_count)))
              OR (interval_secs IS NOT NULL
                  AND end_time IS NOT NULL
                  AND end_time < ?)
           RETURNING id, short_id, name,
                     CASE WHEN interval_secs IS NULL THEN 'completed'
                          ELSE 'expired' END AS reason"#,
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .context("Failed to delete pruned tasks")?;

    Ok(rows
        .iter()
        .map(|row| PrunedTask {
            id: row.get("id"),
            short_id: row.get("short_id"),
            name: row.get("name"),
            reason: row.get("reason"),
        })
        .collect())
}

/// Allocate the smallest free positive short id (>= 1) — the first gap in
/// `short_id` space across all rows. Completed oneshot tasks hold `NULL`
/// short ids, so their former ids are immediately free for reuse.
///
/// The allocation is a read-only query: the id is bound at INSERT/UPDATE
/// time, and the `short_id` column is `UNIQUE`, so a concurrent
/// double-allocation fails loudly rather than silently sharing an id. In
/// practice the CLI is single-threaded per invocation.
pub async fn allocate_short_id(pool: &SqlitePool) -> Result<i64> {
    let taken: Vec<i64> = sqlx::query_scalar(
        "SELECT short_id FROM todos WHERE short_id IS NOT NULL ORDER BY short_id ASC",
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch short ids for allocation")?;
    let mut expected = 1i64;
    for id in taken {
        if id == expected {
            expected += 1;
        } else if id > expected {
            break;
        }
    }
    Ok(expected)
}

/// Ensure a task's `short_id` reflects its completion state:
///
/// * A not-done task must have a short id — when it's `NULL`, the smallest
///   free positive id is allocated (first-available-gap, so untoggling a
///   completion reassigns the task's id).
/// * A done **oneshot** task must not have a short id: it is cleared on
///   completion, freeing the id for reuse. Recurring tasks keep their short
///   id across intervals — their "done" state is interval-scoped and
///   transient, so clearing/reassigning per interval would churn ids.
///
/// Completion state is evaluated with completions scoped to the current
/// interval for recurring tasks (matching [`update_task`]).
pub async fn sync_short_id(pool: &SqlitePool, todo_id: i64) -> Result<()> {
    let row = sqlx::query(
        "SELECT start_time, interval_secs, target_count, short_id FROM todos WHERE id = ?",
    )
    .bind(todo_id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch task for short-id sync")?;
    let Some(row) = row else { return Ok(()) };

    let start_time: Option<i64> = row.get("start_time");
    let interval_secs: Option<i64> = row.get("interval_secs");
    let target_count: i32 = row.get("target_count");
    let short_id: Option<i64> = row.get("short_id");

    let boundary = match (start_time, interval_secs) {
        (Some(st), Some(iv)) if iv > 0 => Some(crate::task::current_interval_start(
            st,
            crate::date::db_to_span(iv),
            crate::date::now(),
        )),
        _ => None,
    };
    let sum: i32 = match boundary {
        Some(b) => sqlx::query_scalar(
            "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ? AND time >= ?",
        )
        .bind(todo_id)
        .bind(b)
        .fetch_one(pool)
        .await?,
        None => {
            sqlx::query_scalar(
                "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
            )
            .bind(todo_id)
            .fetch_one(pool)
            .await?
        }
    };

    // Recurring tasks never lose their short id; only oneshot tasks do.
    if interval_secs.is_some() {
        if short_id.is_none() {
            let new_id = allocate_short_id(pool).await?;
            sqlx::query("UPDATE todos SET short_id = ? WHERE id = ?")
                .bind(new_id)
                .bind(todo_id)
                .execute(pool)
                .await
                .context("Failed to assign short id")?;
        }
        return Ok(());
    }

    let done = crate::task::is_task_done(target_count, Some(sum));
    match (done, short_id) {
        (true, Some(_)) => {
            sqlx::query("UPDATE todos SET short_id = NULL WHERE id = ?")
                .bind(todo_id)
                .execute(pool)
                .await
                .context("Failed to clear short id")?;
        }
        (false, None) => {
            let new_id = allocate_short_id(pool).await?;
            sqlx::query("UPDATE todos SET short_id = ? WHERE id = ?")
                .bind(new_id)
                .bind(todo_id)
                .execute(pool)
                .await
                .context("Failed to assign short id")?;
        }
        _ => {}
    }
    Ok(())
}

/// The current short id of a task (`None` once a oneshot task is completed).
pub async fn fetch_task_short_id(pool: &SqlitePool, id: i64) -> Result<Option<i64>> {
    let short_id: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT short_id FROM todos WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .context("Failed to fetch short id")?;
    Ok(short_id)
}

/// Resolve a user-facing `short_id` to the stable row id plus the name of
/// the task holding it (task-tree parent lookup for `! -<parent_id>`, where
/// the name backs the attach confirmation prompt). `None` when no task holds
/// that short id — completed oneshot tasks hold `NULL`, so they are never
/// resolvable.
pub async fn fetch_task_id_by_short_id(
    pool: &SqlitePool,
    short_id: i64,
) -> Result<Option<(i64, String)>> {
    let row = sqlx::query("SELECT id, name FROM todos WHERE short_id = ?")
        .bind(short_id)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch task by short id")?;
    Ok(row.map(|r| (r.get("id"), r.get("name"))))
}

/// Whether a task with the given name already exists. `task_type` scopes
/// the check to a task kind, using the same column discriminators as the
/// views: recurring tasks have `interval_secs` set, scheduled tasks have
/// `available_duration_secs` set (oneshots have neither). `None` checks
/// every task regardless of kind (global uniqueness).
pub async fn task_name_exists(
    pool: &SqlitePool,
    name: &str,
    task_type: Option<TaskKind>,
) -> Result<bool> {
    let query = match task_type {
        None => "SELECT COUNT(*) FROM todos WHERE name = ?",
        Some(TaskKind::Recurring) => {
            "SELECT COUNT(*) FROM todos WHERE name = ? AND interval_secs IS NOT NULL"
        }
        Some(TaskKind::Oneshot) => {
            "SELECT COUNT(*) FROM todos WHERE name = ? AND interval_secs IS NULL AND available_duration_secs IS NULL"
        }
        Some(TaskKind::Scheduled) => {
            "SELECT COUNT(*) FROM todos WHERE name = ? AND interval_secs IS NULL AND available_duration_secs IS NOT NULL"
        }
    };
    let count: i64 = sqlx::query_scalar::<_, i64>(query)
        .bind(name)
        .fetch_one(pool)
        .await
        .context("Failed to check task name uniqueness")?;
    Ok(count > 0)
}

// ---------------------------------------------------------------------------
// Entry insertion
// ---------------------------------------------------------------------------

/// The full row for one task, with completions scoped to the current
/// interval for recurring tasks (TUI today-view selection).
/// last_time is unscoped.
pub async fn fetch_task_by_id(pool: &SqlitePool, id: i64, now: i64) -> Result<Option<TaskRow>> {
    let row = sqlx::query_as::<_, TaskRow>(
        r#"SELECT t.*, NULL AS completions, NULL AS last_time
           FROM todos t WHERE t.id = ?"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch task")?;

    let Some(mut task) = row else { return Ok(None) };
    // Scope the completion sum to the current interval for recurring tasks
    // (calendar-aware jiff math); last_time stays unscoped.
    let rows =
        sqlx::query("SELECT time, count FROM todo_completions WHERE todo_id = ? ORDER BY time ASC")
            .bind(id)
            .fetch_all(pool)
            .await
            .context("Failed to fetch task completions")?;
    let completions: Vec<super::models::CompletionRow> = rows
        .iter()
        .map(|r| super::models::CompletionRow {
            time: r.get("time"),
            count: r.get("count"),
        })
        .collect();
    task.completions = match crate::task::interval_start(&task, now) {
        Some(floor) => {
            let scoped: Vec<_> = completions.iter().filter(|c| c.time >= floor).collect();
            if scoped.is_empty() {
                None
            } else {
                Some(scoped.iter().map(|c| c.count).sum())
            }
        }
        None => {
            if completions.is_empty() {
                None
            } else {
                Some(completions.iter().map(|c| c.count).sum())
            }
        }
    };
    task.last_time = completions.iter().map(|c| c.time).max();
    Ok(Some(task))
}

/// Task rows by id, with completions scoped like [`fetch_task_by_id`] —
/// the today view's date-column coloring needs the live done state.
/// Absent ids are omitted from the map; an empty input returns an empty
/// map.
pub async fn fetch_tasks_by_ids(
    pool: &SqlitePool,
    ids: &[i64],
    now: i64,
) -> Result<std::collections::HashMap<i64, TaskRow>> {
    let map = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT t.*, NULL AS completions, NULL AS last_time FROM todos t WHERE t.id IN ({})",
        ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(sql.as_str()));
    for id in ids {
        q = q.bind(id);
    }
    let tasks = q
        .fetch_all(pool)
        .await
        .context("Failed to fetch tasks by id")?;
    Ok(super::views::attach_full_completions(pool, tasks, now)
        .await?
        .into_iter()
        .map(|t| (t.id, t))
        .collect())
}

/// Full recurring task (edit flow), looked up by name.
pub async fn fetch_recurring_task_by_name(
    pool: &SqlitePool,
    name: &str,
) -> Result<Option<TaskObject>> {
    let row = sqlx::query(
        r#"SELECT id, name, body, priority, short_id, start_time,
                   available_duration_secs, interval_secs, target_count,
                   optional, end_time, parent
           FROM todos WHERE name = ? AND interval_secs IS NOT NULL"#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch recurring task")?;

    Ok(row.map(|r| TaskObject {
        id: Some(r.get("id")),
        short_id: r.get("short_id"),
        name: r.get("name"),
        body: r.get("body"),
        priority: r.get("priority"),
        start_time: r.get("start_time"),
        available_duration_secs: r.get("available_duration_secs"),
        interval_secs: r.get("interval_secs"),
        target_count: r.get("target_count"),
        optional: r.get::<i32, _>("optional") != 0,
        end_time: r.get("end_time"),
        parent: r.get("parent"),
    }))
}

/// Oneshot task + prior completion count for the `- <short-id> [count]`
/// update command, looked up by its user-facing short id. Completed oneshot
/// tasks have no short id, so they are not addressable by id (use the word
/// query form instead).
pub async fn fetch_oneshot_task_for_update(
    pool: &SqlitePool,
    short_id: i64,
) -> Result<Option<TaskUpdateInfo>> {
    let row = sqlx::query(
        r#"SELECT id, name, target_count, short_id,
                  COALESCE((SELECT SUM(count) FROM todo_completions
                            WHERE todo_id = todos.id), 0) AS prior_completions
           FROM todos WHERE short_id = ? AND interval_secs IS NULL"#,
    )
    .bind(short_id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch task")?;

    Ok(row.map(|r| TaskUpdateInfo {
        id: r.get("id"),
        short_id: r.get("short_id"),
        name: r.get("name"),
        target_count: r.get("target_count"),
        prior_completions: r.get("prior_completions"),
    }))
}

/// Oneshot tasks whose names contain all `words` in order (a subsequence
/// match over whitespace-split words), with prior completion counts — the
/// candidates for the `im - <words…> [count]` update form. The
/// subsequence test is done here in Rust: SQL `LIKE` can't express
/// "in order, with gaps allowed".
pub async fn fetch_oneshot_matching_words(
    pool: &SqlitePool,
    words: &[String],
) -> Result<Vec<TaskUpdateInfo>> {
    let rows = sqlx::query(
        r#"SELECT id, name, target_count, short_id,
                  COALESCE((SELECT SUM(count) FROM todo_completions
                            WHERE todo_id = todos.id), 0) AS prior_completions
           FROM todos WHERE interval_secs IS NULL"#,
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch tasks for word query")?;

    Ok(rows
        .into_iter()
        .filter(|r| {
            let name: String = r.get("name");
            name_contains_words_in_order(&name, words)
        })
        .map(|r| TaskUpdateInfo {
            id: r.get("id"),
            short_id: r.get("short_id"),
            name: r.get("name"),
            target_count: r.get("target_count"),
            prior_completions: r.get("prior_completions"),
        })
        .collect())
}

/// True iff every word in `words` appears in `name` as a whitespace-
/// separated word, in the same relative order (extra words in between are
/// allowed). Empty `words` never matches.
fn name_contains_words_in_order(name: &str, words: &[String]) -> bool {
    if words.is_empty() {
        return false;
    }
    let mut wi = 0;
    for nw in name.split_whitespace() {
        if wi < words.len() && nw == words[wi] {
            wi += 1;
        }
    }
    wi == words.len()
}

/// Update a todo's body. Returns the number of affected rows.
pub async fn update_todo_body(pool: &SqlitePool, id: i64, body: &str) -> Result<u64> {
    let res = sqlx::query("UPDATE todos SET body = ? WHERE id = ?")
        .bind(body)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update task body")?;
    Ok(res.rows_affected())
}

/// Attach a task to a parent task by the parent's raw row id (the
/// today-view Link prompt): replaces the task's `parent` column (NULL
/// when it had none). No id validation — a parent cycle would be clipped
/// by the task-tree load, and a nonexistent parent just leaves an orphan
/// link; callers log the result.
pub async fn set_task_parent(pool: &SqlitePool, task_id: i64, parent_id: i64) -> Result<u64> {
    let res = sqlx::query("UPDATE todos SET parent = ? WHERE id = ?")
        .bind(parent_id)
        .bind(task_id)
        .execute(pool)
        .await
        .context("Failed to set task parent")?;
    Ok(res.rows_affected())
}

/// Set a scheduled task's completion entry, replacing any existing one.
/// Scheduled tasks keep at most one completion row: `value` 1 = completed
/// (early, or auto-completed by window elapse), 0 = failed (marked as
/// missed). Runs in a transaction so the replace is atomic, then syncs the
/// short id (a completed task loses its short id; a failed one keeps it).
pub async fn set_scheduled_completion(pool: &SqlitePool, todo_id: i64, value: i32) -> Result<()> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    sqlx::query("DELETE FROM todo_completions WHERE todo_id = ?")
        .bind(todo_id)
        .execute(&mut *tx)
        .await
        .context("Failed to clear scheduled task completion")?;

    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, ?)")
        .bind(todo_id)
        .bind(crate::date::now())
        .bind(value)
        .execute(&mut *tx)
        .await
        .context("Failed to insert scheduled task completion")?;

    tx.commit().await.context("Failed to commit transaction")?;

    sync_short_id(pool, todo_id).await?;
    Ok(())
}

/// Clear a task's completion progress. For recurring tasks only completions
/// at/after `floor` (the current interval start) are removed, preserving
/// history from earlier intervals. Returns affected rows.
pub async fn reset_task_completions(pool: &SqlitePool, id: i64, floor: Option<i64>) -> Result<u64> {
    let res = match floor {
        Some(floor) => sqlx::query("DELETE FROM todo_completions WHERE todo_id = ? AND time >= ?")
            .bind(id)
            .bind(floor)
            .execute(pool)
            .await
            .context("Failed to reset task progress")?,
        None => sqlx::query("DELETE FROM todo_completions WHERE todo_id = ?")
            .bind(id)
            .execute(pool)
            .await
            .context("Failed to reset task progress")?,
    };
    // Removing completion rows may untoggle a completed task — sync its
    // short id (a not-done task is reassigned the smallest free id).
    sync_short_id(pool, id).await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::db::{create_task, TaskObject};

    /// Seed a root-level task; returns its id.
    async fn seed_task(pool: &SqlitePool, name: &str) -> i64 {
        let (id, _) = create_task(
            pool,
            &TaskObject {
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
            },
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn test_set_task_parent() {
        let pool = test_pool().await.unwrap();
        let parent = seed_task(&pool, "parent").await;
        let child = seed_task(&pool, "child").await;
        let now = crate::date::now();

        // Attach (the child had no parent).
        assert_eq!(set_task_parent(&pool, child, parent).await.unwrap(), 1);
        let row = fetch_task_by_id(&pool, child, now).await.unwrap().unwrap();
        assert_eq!(row.parent, Some(parent));

        // Re-attach under a second parent: replaces the first.
        let parent2 = seed_task(&pool, "parent2").await;
        assert_eq!(set_task_parent(&pool, child, parent2).await.unwrap(), 1);
        let row = fetch_task_by_id(&pool, child, now).await.unwrap().unwrap();
        assert_eq!(row.parent, Some(parent2));

        // Nonexistent task id → 0 rows affected.
        assert_eq!(set_task_parent(&pool, 9999, parent).await.unwrap(), 0);
    }
}
