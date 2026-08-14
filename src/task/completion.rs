use sqlx::SqlitePool;

/// Apply a completion delta to a list of per-event counts (most recent last).
///
/// - delta > 0: append a new entry with that count.
/// - delta < 0: consume entries from the end while remaining > 0 — if the
///   last entry's count is >= remaining, reduce it by remaining; otherwise
///   remove the entry entirely and subtract its count from remaining.
/// - delta == 0: unchanged.
///
/// Counts are never negative: negative deltas only remove/reduce existing
/// entries, so at read time the total is always >= 0.
pub fn apply_delta_to_counts(counts: &[i32], delta: i32) -> Vec<i32> {
    if delta > 0 {
        let mut out = counts.to_vec();
        out.push(delta);
        out
    } else if delta < 0 {
        let mut remaining = -delta;
        let mut out = counts.to_vec();
        while remaining > 0 {
            match out.pop() {
                Some(count) if count > remaining => {
                    out.push(count - remaining);
                    remaining = 0;
                }
                Some(count) => remaining -= count,
                None => break,
            }
        }
        out
    } else {
        counts.to_vec()
    }
}

/// Apply a completion delta to a task at write time, keeping the per-event
/// counts in `todo_completions` as the single source of truth.
///
/// Positive deltas append a new entry with that count; negative deltas
/// consume the most recent entries (see [`apply_delta_to_counts`]). Returns
/// the new total (SUM of counts), which is always >= 0.
///
/// For recurring tasks the consumption is bounded to the current interval:
/// entries from before the current interval started are never touched, and
/// the returned total is the sum within the current interval only.
///
/// After applying the delta the task's `short_id` is synced to its
/// completion state: a oneshot task that just completed loses its short id;
/// a oneshot task that just became not-done again is reassigned the
/// smallest free one.
///
/// The SQL lives in `crate::db::update_task`; this wrapper keeps the
/// task-completion API at `task::` for callers and tests.
pub async fn apply_completion_delta(
    pool: &SqlitePool,
    todo_id: i64,
    delta: i32,
) -> anyhow::Result<i32> {
    crate::db::update_task(pool, todo_id, delta).await
}

/// Check if a task is considered "done" based on its target_count and completions.
///
/// - target_count == 0: Simple done/not-done. Done if completions > 0.
///   (`Some(0)` is *not* done — zero completions is the not-done state regardless
///   of target_count.)
/// - target_count > 0: Needs N completions. Done if completions >= target_count.
pub fn is_task_done(target_count: i32, completions: Option<i32>) -> bool {
    match completions {
        None => false,
        Some(0) => false,
        Some(count) => {
            if target_count == 0 {
                true
            } else {
                count >= target_count
            }
        }
    }
}

/// Calculate the completion percentage for a task.
/// Returns None if target_count is 0 (simple done/not-done).
/// Returns Some(percentage) if target_count > 0.
pub fn completion_percentage(target_count: i32, completions: Option<i32>) -> Option<f64> {
    if target_count == 0 {
        None
    } else {
        let count = completions.unwrap_or(0);
        Some((count as f64 / target_count as f64) * 100.0)
    }
}
