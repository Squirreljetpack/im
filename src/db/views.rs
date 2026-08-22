use anyhow::{Context, Result};
use sqlx::{FromRow, Row, SqlitePool};

use super::entries::fetch_completions_between;
use super::models::{CompletionRow, RecurringWindow, TaskRow};
use crate::types::{TasksFilter, ViewMode, ViewVariant};

/// Oneshot tasks per the today view's [`TasksFilter`]. Bounds are on the
/// due time (`end_time` when set — `! name @<time>`; rows without one fall
/// back to `start_time`, so legacy rows and undated tasks keep a proxy
/// due): `Horizon` keeps only [day_start, horizon_end] (overdue excluded),
/// `Overdue` keeps dated rows (`end_time` set) due by the horizon end,
/// `Pending` and `All` have no date bounds at all. Every filter drops
/// completed rows — the today view surfaces today's completions through
/// the separate completed-today fetch (see `fetch_tasks_completed_on`),
/// so completed tasks never linger in the regular lists. `None`
/// (journal-only mode) returns no rows — the today view skips the fetch
/// entirely, so this is a defensive short-circuit. The bounds are bound
/// (i64::MIN / i64::MAX = effectively no filter) so the SQL stays static.
pub async fn fetch_oneshot_tasks(
    pool: &SqlitePool,
    filter: TasksFilter,
    horizon_end: i64,
    day_start: i64,
) -> Result<Vec<TaskRow>> {
    // available_duration_secs IS NULL excludes scheduled tasks — the today
    // view fetches those separately via fetch_scheduled_today.
    let tasks = match filter {
        // Journal-only mode: the today view skips this fetch entirely, so
        // this arm is a defensive short-circuit.
        TasksFilter::None => return Ok(Vec::new()),
        TasksFilter::All | TasksFilter::Pending => sqlx::query_as::<_, TaskRow>(
            r#"SELECT t.*, NULL AS completions, NULL AS last_time
               FROM todos t
               WHERE t.interval_secs IS NULL
               AND t.available_duration_secs IS NULL
               ORDER BY t.priority DESC, COALESCE(t.end_time, t.start_time) ASC"#,
        )
        .fetch_all(pool)
        .await
        .context("Failed to fetch oneshot tasks")?,
        TasksFilter::Horizon => sqlx::query_as::<_, TaskRow>(
            r#"SELECT t.*, NULL AS completions, NULL AS last_time
               FROM todos t
               WHERE t.interval_secs IS NULL
               AND t.available_duration_secs IS NULL
               AND COALESCE(t.end_time, t.start_time) <= ?
               AND COALESCE(t.end_time, t.start_time) >= ?
               ORDER BY t.priority DESC, COALESCE(t.end_time, t.start_time) ASC"#,
        )
        .bind(horizon_end)
        .bind(day_start)
        .fetch_all(pool)
        .await
        .context("Failed to fetch horizon oneshot tasks")?,
        // Strict `end_time` bound: undated tasks are never overdue and have
        // no due-in-horizon moment, so they don't belong to this filter.
        TasksFilter::Due => sqlx::query_as::<_, TaskRow>(
            r#"SELECT t.*, NULL AS completions, NULL AS last_time
               FROM todos t
               WHERE t.interval_secs IS NULL
               AND t.available_duration_secs IS NULL
               AND t.end_time IS NOT NULL
               AND t.end_time <= ?
               ORDER BY t.priority DESC, t.end_time ASC"#,
        )
        .bind(horizon_end)
        .fetch_all(pool)
        .await
        .context("Failed to fetch due oneshot tasks")?,
    };

    let with_completions = attach_full_completions(pool, tasks, crate::date::now()).await?;
    // Incomplete only — completed tasks surface through the today view's
    // completed-today fetch, so `All` shows open oneshots plus whatever
    // was completed today, not every oneshot ever completed.
    Ok(with_completions
        .into_iter()
        .filter(|t| !t.is_done())
        .collect())
}

/// Scheduled tasks whose availability window overlaps `[floor, horizon_end)`:
/// started before the horizon ends and still open past the floor. Used by the
/// today view (floor = today start, horizon_end scales with the horizon). All
/// states are included — ongoing, completed, failed — matching the window
/// overlap semantics in VIEWS.md.
pub async fn fetch_scheduled_today(
    pool: &SqlitePool,
    horizon_end: i64,
    floor: i64,
) -> Result<Vec<TaskRow>> {
    let tasks = sqlx::query_as::<_, TaskRow>(
        r#"SELECT t.*, NULL AS completions, NULL AS last_time
           FROM todos t
           WHERE t.interval_secs IS NULL
             AND t.available_duration_secs IS NOT NULL
             AND t.start_time < ?
             AND t.start_time + t.available_duration_secs > ?
           ORDER BY t.priority DESC, t.start_time ASC"#,
    )
    .bind(horizon_end)
    .bind(floor)
    .fetch_all(pool)
    .await
    .context("Failed to fetch scheduled tasks for today")?;

    let with_completions = attach_full_completions(pool, tasks, crate::date::now()).await?;
    Ok(with_completions)
}

/// Whether a recurring task is currently within its availability window.
/// Tasks without an `available_duration_secs` are always available.
///
/// Expired tasks (end_time set and past) are *not* subject to the window
/// check: they have no current interval, so expiry is handled by the SQL
/// `end_time` filter instead (they pass through here).
pub fn recurring_available(task: &TaskRow, now: i64) -> bool {
    if task.end_time.is_some_and(|end| now > end) {
        return true;
    }
    match (
        task.start_time,
        task.interval_span(),
        task.available_duration_secs,
    ) {
        (Some(st), Some(span), Some(dur)) if dur < crate::date::span_rough_seconds(span) as i64 => {
            let interval_start = crate::task::current_interval_start(st, span, now);
            now - interval_start < dur
        }
        _ => true,
    }
}

/// Tasks with a completion entry in `[day_start, day_end)` — the
/// "completed today" fetch for the today view. The completions sum is
/// scoped to the recurring task's current interval (so the badge matches
/// the regular recurring fetch, D8); non-recurring rows keep the unscoped
/// sum. `last_time` is the most recent completion timestamp within the day
/// window (the time label + sort key for the merged row).
pub async fn fetch_tasks_completed_on(
    pool: &SqlitePool,
    day_start: i64,
    day_end: i64,
) -> Result<Vec<TaskRow>> {
    let tasks = sqlx::query_as::<_, TaskRow>(
        r#"SELECT t.*, NULL AS completions, NULL AS last_time
           FROM todos t
           WHERE EXISTS (
               SELECT 1 FROM todo_completions c
               WHERE c.todo_id = t.id AND c.time >= ? AND c.time < ?
           )
           ORDER BY t.priority DESC, t.start_time ASC"#,
    )
    .bind(day_start)
    .bind(day_end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch tasks completed today")?;

    if tasks.is_empty() {
        return Ok(tasks);
    }

    let rows = fetch_completions_in_window(pool, &tasks, day_start, day_end).await?;
    let now = crate::date::now();
    Ok(tasks
        .into_iter()
        .map(|task| {
            let rows = rows.get(&task.id).cloned().unwrap_or_default();
            // Scoped sum for recurring (current interval), full otherwise.
            let scoped = scoped_completion_sum(&task, &rows, now);
            // The day-window last completion (the time label).
            let last_time = rows.iter().map(|c| c.time).max();
            TaskRow {
                completions: scoped,
                last_time,
                ..task
            }
        })
        .collect())
}

/// Fetch completion events in `[start, end]` (inclusive) along with their task row,
/// where each task row's `completions` reflects the cumulative completion count at that
/// completion event's time (and `last_time` is the completion event's timestamp).
pub async fn fetch_completion_events_in_range(
    pool: &SqlitePool,
    start: i64,
    end: i64,
) -> Result<Vec<(CompletionRow, TaskRow)>> {
    let rows = sqlx::query(
        r#"SELECT c.id AS completion_id, c.time AS completion_time, c.count AS completion_count,
                  t.id, t.short_id, t.name, t.body, t.priority, t.start_time,
                  t.available_duration_secs, t.interval_secs, t.target_count, t.optional,
                  t.end_time, t.parent
           FROM todo_completions c
           JOIN todos t ON t.id = c.todo_id
           WHERE c.time >= ? AND c.time <= ?
           ORDER BY c.time ASC, c.id ASC"#,
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch completion events in range")?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut events = Vec::with_capacity(rows.len());
    let mut task_map: std::collections::HashMap<i64, TaskRow> = std::collections::HashMap::new();

    for row in rows {
        let comp = CompletionRow {
            time: row.get("completion_time"),
            count: row.get("completion_count"),
        };
        let task_id: i64 = row.get("id");
        task_map.entry(task_id).or_insert_with(|| TaskRow {
            id: task_id,
            short_id: row.get("short_id"),
            name: row.get("name"),
            body: row.get("body"),
            priority: row.get("priority"),
            start_time: row.get("start_time"),
            available_duration_secs: row.get("available_duration_secs"),
            interval_secs: row.get("interval_secs"),
            target_count: row.get("target_count"),
            optional: row.get("optional"),
            end_time: row.get("end_time"),
            parent: row.get("parent"),
            completions: None,
            last_time: None,
        });
        events.push((comp, task_id));
    }

    let tasks_vec: Vec<TaskRow> = task_map.values().cloned().collect();
    let all_completions = fetch_completions_for_tasks(pool, &tasks_vec).await?;

    let mut results = Vec::with_capacity(events.len());
    for (comp, task_id) in events {
        if let Some(base_task) = task_map.get(&task_id) {
            let task_rows = all_completions.get(&task_id).map(|v| v.as_slice()).unwrap_or(&[]);
            let cumulative = if base_task.is_recurring() {
                match crate::task::interval_start(base_task, comp.time) {
                    Some(floor) => task_rows
                        .iter()
                        .filter(|c| c.time >= floor && c.time <= comp.time)
                        .map(|c| c.count)
                        .sum(),
                    None => task_rows
                        .iter()
                        .filter(|c| c.time <= comp.time)
                        .map(|c| c.count)
                        .sum(),
                }
            } else {
                task_rows
                    .iter()
                    .filter(|c| c.time <= comp.time)
                    .map(|c| c.count)
                    .sum()
            };

            let mut task = base_task.clone();
            task.completions = Some(cumulative);
            task.last_time = Some(comp.time);
            results.push((comp, task));
        }
    }

    Ok(results)
}

/// Availability windows of a recurring task that intersect
/// `[period_start, period_end]`, as `(window_start, window_end)` pairs
/// (ascending). Windows are `[start + k*interval, start + k*interval +
/// dur)` — the whole interval when `dur` is None or >= interval — and
/// move with each interval, so the scan is built from `interval_start`
/// math: a raw `start_time + duration >= period_start` comparison would
/// degenerate to "every task ever started" for old start times. When
/// `end_time` is set, the last window is truncated at the expiry and
/// windows after it don't count.
fn recurring_windows_in_period(
    task: &TaskRow,
    period_start: i64,
    period_end: i64,
) -> Vec<(i64, i64)> {
    let (Some(st), Some(span)) = (task.start_time, task.interval_span()) else {
        return Vec::new();
    };
    if crate::date::span_rough_seconds(span) <= 0.0 {
        return Vec::new();
    }
    // Window length: explicit duration when set and < interval, else the
    // whole interval.
    let dur = match task.available_duration_secs {
        Some(d) if d < crate::date::span_rough_seconds(span) as i64 => Some(d),
        _ => None,
    };
    // First window index that could reach the period (overshoot by one
    // window; k >= 0).
    let (Ok(anchor_z), Ok(period_start_z)) = (
        crate::date::zoned_from_unix_secs(st),
        crate::date::zoned_from_unix_secs(period_start),
    ) else {
        return Vec::new();
    };
    let k0 =
        (crate::date::interval_index(&anchor_z, &period_start_z, span).unwrap_or(1) - 1).max(0);
    let mut windows = Vec::new();
    let mut k = k0;
    loop {
        let Ok(span_k) = span.checked_mul(k) else {
            break;
        };
        let Ok(w_start_z) = anchor_z.checked_add(span_k) else {
            break;
        };
        let w_start = w_start_z.timestamp().as_second();
        if w_start > period_end {
            break;
        }
        let w_end_unbounded = w_start_z
            .checked_add(span)
            .map(|z| z.timestamp().as_second())
            .unwrap_or(w_start);
        let w_end = match (dur, task.end_time) {
            (Some(d), Some(end)) => (w_start + d).min(end),
            (Some(d), None) => w_start + d,
            (None, Some(end)) => w_end_unbounded.min(end),
            (None, None) => w_end_unbounded,
        };
        if w_end > period_start {
            windows.push((w_start, w_end));
        }
        k += 1;
    }
    windows
}

/// A recurring task row plus its unscoped last completion — the today-view
/// window fetch returns it so each window row can carry the unscoped last
/// in `end_time`.
#[derive(Debug, FromRow)]
struct RecurringTaskRow {
    #[sqlx(flatten)]
    task: TaskRow,
    unscoped_last: Option<i64>,
}

/// Per-availability-window rows for every recurring task with a window
/// intersecting `[period_start, period_end]` — the today-view recurring
/// fetch (all variants). One [`RecurringWindow`] per intersecting window;
/// the view decides whether to keep them all (All) or only the next per
/// task (B). Completions are scoped per window's interval (sum + most
/// recent completion time), matching the interval-scoped completion
/// queries elsewhere (D8). Expired tasks (`end_time` passed before the
/// period) are excluded, they have no windows in it.
pub async fn fetch_recurring_windows_for_period(
    pool: &SqlitePool,
    period_start: i64,
    period_end: i64,
) -> Result<Vec<RecurringWindow>> {
    let tasks = sqlx::query_as::<_, RecurringTaskRow>(
        r#"SELECT t.*, NULL AS completions, NULL AS last_time,
                  (SELECT MAX(tc.time) FROM todo_completions tc
                       WHERE tc.todo_id = t.id) AS unscoped_last
           FROM todos t
           WHERE t.interval_secs IS NOT NULL
           AND (t.end_time IS NULL OR t.end_time > ?)
           AND t.start_time <= ?
           ORDER BY t.priority DESC, t.start_time ASC"#,
    )
    .bind(period_start)
    .bind(period_end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch recurring tasks for period")?;

    let mut windows = Vec::new();
    for row in &tasks {
        let task = &row.task;
        let wins = recurring_windows_in_period(task, period_start, period_end);
        if wins.is_empty() {
            continue;
        }
        let span = task.interval_span().expect("filtered to recurring tasks");
        let st = task.start_time.expect("filtered to tasks with a start");
        // Completion events within the span of the intersecting windows
        // (each window's interval, i.e. up to the last interval end).
        let span_end =
            wins.last().expect("non-empty").0 + crate::date::span_rough_seconds(span) as i64;
        let completions = fetch_completions_between(pool, task.id, wins[0].0, span_end).await?;
        let k_first = crate::date::interval_index(
            &crate::date::zoned_from_unix_secs(st).expect("st is a valid epoch"),
            &crate::date::zoned_from_unix_secs(wins[0].0).expect("window start is valid"),
            span,
        )
        .unwrap_or(0);
        for (wi, (w_start, w_end)) in wins.iter().enumerate() {
            let mut count = 0i32;
            let mut last_time: Option<i64> = None;
            for c in &completions {
                let k = crate::date::interval_index(
                    &crate::date::zoned_from_unix_secs(st).expect("st is a valid epoch"),
                    &crate::date::zoned_from_unix_secs(c.time).expect("completion is valid"),
                    span,
                )
                .unwrap_or(i64::MAX);
                if k == k_first + wi as i64 {
                    count += c.count;
                    last_time = Some(c.time);
                }
            }
            let mut task = task.clone();
            task.completions = Some(count);
            task.last_time = last_time;
            // The window row's `end_time` carries the task's unscoped last
            // completion (the today view doesn't use the expiry; the window
            // geometry above was computed against the real end_time).
            task.end_time = row.unscoped_last;
            windows.push(RecurringWindow {
                task,
                window_start: *w_start,
                window_end: *w_end,
            });
        }
    }
    Ok(windows)
}

// ---------------------------------------------------------------------------
// Shared completion-aggregation helpers (Rust-side scoping)
// ---------------------------------------------------------------------------

/// Fetch all completion rows for the given tasks (`todo_id` → rows, by
/// time). Empty when `tasks` is empty.
pub async fn fetch_completions_for_tasks(
    pool: &SqlitePool,
    tasks: &[TaskRow],
) -> Result<std::collections::HashMap<i64, Vec<CompletionRow>>> {
    let mut map = std::collections::HashMap::new();
    if tasks.is_empty() {
        return Ok(map);
    }
    let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
    let sql = format!(
        "SELECT todo_id, time, count FROM todo_completions WHERE todo_id IN ({}) ORDER BY time ASC",
        ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q
        .fetch_all(pool)
        .await
        .context("Failed to fetch task completions")?;
    for row in rows {
        map.entry(row.get::<i64, _>("todo_id"))
            .or_insert_with(Vec::new)
            .push(CompletionRow {
                time: row.get("time"),
                count: row.get("count"),
            });
    }
    Ok(map)
}

/// Completion rows of the given tasks within `[start, end)` (`todo_id` →
/// rows, by time).
async fn fetch_completions_in_window(
    pool: &SqlitePool,
    tasks: &[TaskRow],
    start: i64,
    end: i64,
) -> Result<std::collections::HashMap<i64, Vec<CompletionRow>>> {
    let mut map = std::collections::HashMap::new();
    if tasks.is_empty() {
        return Ok(map);
    }
    let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
    let sql = format!(
        "SELECT todo_id, time, count FROM todo_completions \
         WHERE todo_id IN ({}) AND time >= ? AND time < ? ORDER BY time ASC",
        ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await
        .context("Failed to fetch task completions")?;
    for row in rows {
        map.entry(row.get::<i64, _>("todo_id"))
            .or_insert_with(Vec::new)
            .push(CompletionRow {
                time: row.get("time"),
                count: row.get("count"),
            });
    }
    Ok(map)
}

/// Completion sum over `rows`; `None` when there are no rows (the SQL
/// `SUM` over an empty set yields NULL — the `completions IS NULL` tests
/// in the view filters depend on the distinction).
fn completion_sum(rows: &[CompletionRow]) -> Option<i32> {
    if rows.is_empty() {
        None
    } else {
        Some(rows.iter().map(|c| c.count).sum())
    }
}

/// The completion sum scoped to the task's current interval (recurring
/// tasks only; others get the full sum). `None` when no completion falls
/// in scope.
fn scoped_completion_sum(task: &TaskRow, rows: &[CompletionRow], now: i64) -> Option<i32> {
    match crate::task::interval_start(task, now) {
        Some(floor) => {
            let scoped: Vec<&CompletionRow> = rows.iter().filter(|c| c.time >= floor).collect();
            if scoped.is_empty() {
                None
            } else {
                Some(scoped.iter().map(|c| c.count).sum())
            }
        }
        None => completion_sum(rows),
    }
}

/// The most recent completion time overall.
fn unscoped_last(rows: &[CompletionRow]) -> Option<i64> {
    rows.iter().map(|c| c.time).max()
}

/// The most recent completion time within the task's current interval
/// (recurring tasks only; others get the overall last).
fn scoped_last(task: &TaskRow, rows: &[CompletionRow], now: i64) -> Option<i64> {
    match crate::task::interval_start(task, now) {
        Some(floor) => rows
            .iter()
            .filter(|c| c.time >= floor)
            .map(|c| c.time)
            .max(),
        None => unscoped_last(rows),
    }
}

/// Attach completion aggregates to a set of already-fetched task rows:
/// `completions` scoped to the current interval for recurring tasks (full
/// sum otherwise), `last_time` the most recent completion overall.
pub async fn attach_full_completions(
    pool: &SqlitePool,
    tasks: Vec<TaskRow>,
    now: i64,
) -> Result<Vec<TaskRow>> {
    if tasks.is_empty() {
        return Ok(tasks);
    }
    let completions = fetch_completions_for_tasks(pool, &tasks).await?;
    Ok(tasks
        .into_iter()
        .map(|task| {
            let rows = completions.get(&task.id).cloned().unwrap_or_default();
            let completions = scoped_completion_sum(&task, &rows, now);
            let last_time = unscoped_last(&rows);
            TaskRow {
                completions,
                last_time,
                ..task
            }
        })
        .collect())
}

/// Task rows for a view mode at a [`ShowVariant`]. The SQL fetch keeps only
/// the kind/availability filters; completion scoping and the
/// completion-based selection (the old `HAVING` clauses) run in Rust with
/// jiff interval math (calendar-aware for recurring tasks). See VIEWS.md
/// for the full matrix.
pub async fn fetch_tasks_for_view(
    pool: &SqlitePool,
    mode: ViewMode,
    show: ViewVariant,
    persist_pending_seconds: i64,
) -> Result<Vec<TaskRow>> {
    let now = crate::date::now();
    let (tasks, done_sort) = match (mode, show) {
        // `@` All: not-done oneshots ∪ recurring (not expired,
        // availability-filtered in Rust) ∪ `ongoing(S)` only (D1:
        // failed/auto-completed/completed scheduled excluded) ∪ any task
        // with a completion entry in the last persist_pending_seconds (D9).
        (ViewMode::PendingTasks, ViewVariant::All) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE (t.interval_secs IS NULL AND t.available_duration_secs IS NULL)
                      OR (t.interval_secs IS NOT NULL AND (t.end_time IS NULL OR t.end_time > ?))
                      OR (t.interval_secs IS NULL AND t.available_duration_secs IS NOT NULL
                          AND t.start_time + t.available_duration_secs >= ?)
                      OR t.id IN (SELECT todo_id FROM todo_completions
                                  WHERE time >= ? AND time <= ?)
                   ORDER BY t.priority DESC, t.start_time ASC"#,
            )
            .bind(now)
            .bind(now)
            .bind(now - persist_pending_seconds)
            .bind(now)
            .fetch_all(pool)
            .await
            .context("Failed to fetch pending tasks")?;
            (tasks, false)
        }
        // `@` A: not-done oneshot tasks only (old `!` list). The D9
        // recently-completed union is intentionally NOT in the SQL here:
        // the old HAVING-only check could only match rows already in the
        // oneshot set (the SQL `WHERE` restricted the set before the
        // HAVING ran), so a recent completion keeps a done oneshot visible
        // but never surfaces recurring/scheduled rows. The Rust filter
        // below applies the same rule (`recent` only matches oneshots).
        (ViewMode::PendingTasks, ViewVariant::A) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE t.interval_secs IS NULL AND t.available_duration_secs IS NULL
                   ORDER BY t.priority DESC, t.start_time ASC"#,
            )
            .fetch_all(pool)
            .await
            .context("Failed to fetch pending oneshot tasks")?;
            (tasks, false)
        }
        // `@` B: not-done recurring (any not expired, NOT availability-
        // filtered — tasks whose availability window has passed stay) ∪
        // non-done scheduled with `window_open` (`now <= start + duration`
        // — ongoing or failed-with-open-window; failed with a closed window
        // belongs to @done) ∪ D9: sched/recur tasks (incl. done) with a
        // completion entry in the last persist_pending_seconds.
        (ViewMode::PendingTasks, ViewVariant::B) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE (t.interval_secs IS NOT NULL AND (t.end_time IS NULL OR t.end_time > ?))
                      OR (t.interval_secs IS NULL AND t.available_duration_secs IS NOT NULL
                          AND t.start_time + t.available_duration_secs >= ?)
                      OR (t.id IN (SELECT todo_id FROM todo_completions
                                   WHERE time >= ? AND time <= ?)
                          AND (t.interval_secs IS NOT NULL
                               OR t.available_duration_secs IS NOT NULL))
                   ORDER BY t.priority DESC, t.start_time ASC"#,
            )
            .bind(now)
            .bind(now)
            .bind(now - persist_pending_seconds)
            .bind(now)
            .fetch_all(pool)
            .await
            .context("Failed to fetch pending recurring/scheduled tasks")?;
            (tasks, false)
        }
        // `@done` All: done oneshots ∪ scheduled with any entry (completed
        // or failed — D2) ∪ recurring done in the current interval.
        (ViewMode::DoneTasks, ViewVariant::All) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE t.interval_secs IS NULL
                      OR (t.interval_secs IS NOT NULL
                          AND (t.start_time IS NULL OR t.start_time <= ?)
                          AND (t.end_time IS NULL OR t.end_time > ?))"#,
            )
            .bind(now)
            .bind(now)
            .fetch_all(pool)
            .await
            .context("Failed to fetch done tasks")?;
            (tasks, true)
        }
        // `@done` A: done oneshot tasks only (`completions >= target_count`).
        (ViewMode::DoneTasks, ViewVariant::A) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE t.interval_secs IS NULL AND t.available_duration_secs IS NULL"#,
            )
            .fetch_all(pool)
            .await
            .context("Failed to fetch done oneshot tasks")?;
            (tasks, true)
        }
        // `@done` B: ALL recurring tasks (one row per task — history scope,
        // no completions filter, includes expired and never-completed rows;
        // D3) ∪ scheduled with any entry or auto-completed (no entry,
        // window elapsed — D2). Completions here are the FULL history sum
        // (no interval scoping — the old query had no scoped join).
        (ViewMode::DoneTasks, ViewVariant::B) => {
            let tasks = sqlx::query_as::<_, TaskRow>(
                r#"SELECT t.*, NULL AS completions, NULL AS last_time
                   FROM todos t
                   WHERE t.interval_secs IS NOT NULL
                      OR (t.interval_secs IS NULL AND t.available_duration_secs IS NOT NULL)"#,
            )
            .fetch_all(pool)
            .await
            .context("Failed to fetch done recurring/scheduled tasks")?;
            (tasks, true)
        }
    };

    let completions = fetch_completions_for_tasks(pool, &tasks).await?;
    // D9: task ids with a completion entry in the persist window.
    let recent_ids: std::collections::HashSet<i64> = if persist_pending_seconds > 0 {
        let rows = sqlx::query(
            "SELECT DISTINCT todo_id FROM todo_completions WHERE time >= ? AND time <= ?",
        )
        .bind(now - persist_pending_seconds)
        .bind(now)
        .fetch_all(pool)
        .await
        .context("Failed to fetch recent completions")?;
        rows.iter().map(|r| r.get::<i64, _>("todo_id")).collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut out: Vec<TaskRow> = Vec::with_capacity(tasks.len());
    for mut task in tasks {
        let rows = completions.get(&task.id).cloned().unwrap_or_default();
        let is_recurring = task.is_recurring();
        let is_scheduled = task.is_scheduled();
        let scoped = scoped_completion_sum(&task, &rows, now);
        let last = if mode == ViewMode::DoneTasks && show == ViewVariant::B {
            // @done:b is history scope: full sums, no interval scoping.
            completion_sum(&rows)
        } else {
            scoped
        };
        let last_time = unscoped_last(&rows);
        task.completions = last;
        task.last_time = last_time;

        let recent = recent_ids.contains(&task.id);
        let keep = match (mode, show) {
            (ViewMode::PendingTasks, ViewVariant::All) => {
                let not_done = scoped.is_none() || scoped.unwrap_or(0) < task.target_count;
                if is_scheduled {
                    scoped.is_none() || recent
                } else {
                    not_done || recent
                }
            }
            (ViewMode::PendingTasks, ViewVariant::A) => {
                let not_done = scoped.is_none() || scoped.unwrap_or(0) < task.target_count;
                not_done || recent
            }
            (ViewMode::PendingTasks, ViewVariant::B) => {
                let not_done = scoped.is_none() || scoped.unwrap_or(0) < task.target_count;
                if is_scheduled {
                    (scoped.is_none() || scoped == Some(0))
                        && task
                            .start_time
                            .is_some_and(|st| st + task.available_duration_secs.unwrap_or(0) >= now)
                        || recent
                } else {
                    not_done || recent
                }
            }
            (ViewMode::DoneTasks, ViewVariant::All) => {
                if is_scheduled {
                    scoped.is_some()
                } else {
                    scoped.is_some_and(|s| s >= task.target_count)
                }
            }
            (ViewMode::DoneTasks, ViewVariant::A) => scoped.is_some_and(|s| s >= task.target_count),
            (ViewMode::DoneTasks, ViewVariant::B) => {
                if is_scheduled {
                    scoped.is_some()
                        || task
                            .start_time
                            .is_some_and(|st| st + task.available_duration_secs.unwrap_or(0) < now)
                } else {
                    true
                }
            }
        };

        if keep
            && !(is_recurring
                && mode == ViewMode::PendingTasks
                && show == ViewVariant::All
                && !recurring_available(&task, now))
        {
            out.push(task);
        }
    }

    if done_sort {
        // Order key mirrors the old SQL: the interval-scoped last completion
        // for recurring rows (the scoped join's MAX), else the overall last;
        // fall back to the implied completion moment (start + duration for
        // scheduled, start for recurring history rows).
        let sort_key = |task: &TaskRow, rows: &[CompletionRow]| -> i64 {
            let scoped = if mode == ViewMode::DoneTasks && show == ViewVariant::B {
                None
            } else {
                scoped_last(task, rows, now)
            };
            let fallback = if task.is_scheduled() {
                task.start_time
                    .unwrap_or(i64::MAX)
                    .saturating_add(task.available_duration_secs.unwrap_or(0))
            } else {
                task.start_time.unwrap_or(i64::MAX)
            };
            scoped.unwrap_or(fallback)
        };
        let mut keyed: Vec<(i64, TaskRow)> = out
            .into_iter()
            .map(|task| {
                let rows = completions.get(&task.id).cloned().unwrap_or_default();
                let key = sort_key(&task, &rows);
                (key, task)
            })
            .collect();
        keyed.sort_by_key(|k| std::cmp::Reverse(k.0));
        out = keyed.into_iter().map(|(_, t)| t).collect();
    }

    Ok(out)
}
