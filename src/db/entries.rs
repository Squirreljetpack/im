use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use super::models::{
    CompletionRow, EntryObject, MoodRow, RecurringTaskMeta, TaskRow, TrackerEntryRow,
    TrackerScoreKindRow, TrackerValue,
};
use super::views::attach_full_completions;

/// Insert a mood entry and its linked tracker values in one transaction.
/// For Text/Float interval trackers, `replace_slot` deletes the previous
/// entry in the same interval slot before inserting. Returns the mood
/// row id, or `None` when no mood row was inserted (tracker-only entry).
pub async fn create_entry(pool: &SqlitePool, entry: &EntryObject) -> Result<Option<i64>> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    let insert_mood = !entry.mood.is_empty()
        || !entry.body.is_empty()
        || entry.duration.is_some()
        || entry.todo_id.is_some();
    let mood_id: Option<i64> = if insert_mood {
        let id: i64 = if let Some(blob) = &entry.embedding {
            sqlx::query(
                "INSERT INTO mood (mood, body, time, embedding, score, duration, todo_id) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
            )
            .bind(&entry.mood)
            .bind(&entry.body)
            .bind(entry.time)
            .bind(blob)
            .bind(entry.score)
            .bind(entry.duration)
            .bind(entry.todo_id)
            .fetch_one(&mut *tx)
            .await
            .context("Failed to insert mood")?
            .get("id")
        } else {
            sqlx::query(
                "INSERT INTO mood (mood, body, time, score, duration, todo_id) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
            )
            .bind(&entry.mood)
            .bind(&entry.body)
            .bind(entry.time)
            .bind(entry.score)
            .bind(entry.duration)
            .bind(entry.todo_id)
            .fetch_one(&mut *tx)
            .await
            .context("Failed to insert mood")?
            .get("id")
        };
        Some(id)
    } else {
        None
    };

    for tracker in &entry.trackers {
        if let Some((slot_start, slot_end)) = tracker.replace_slot {
            sqlx::query("DELETE FROM tracker WHERE type = ? AND time >= ? AND time < ?")
                .bind(&tracker.tracker_type)
                .bind(slot_start)
                .bind(slot_end)
                .execute(&mut *tx)
                .await
                .with_context(|| {
                    format!(
                        "Failed to delete old entry for tracker '{}' in slot {}..{}",
                        tracker.tracker_type, slot_start, slot_end
                    )
                })?;
        }

        let mut q =
            sqlx::query("INSERT INTO tracker (type, score, time, mood) VALUES (?, ?, ?, ?)")
                .bind(&tracker.tracker_type);
        q = match &tracker.value {
            TrackerValue::Text(s) => q.bind(s),
            TrackerValue::Integer(n) => q.bind(n),
            TrackerValue::Float(f) => q.bind(f),
        };
        q.bind(entry.time)
            .bind(mood_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to insert tracker '{}'", tracker.tracker_type))?;
    }

    tx.commit().await.context("Failed to commit transaction")?;
    Ok(mood_id)
}

/// Count mood entries in `[start_time, end_time]`; when `delete` is true,
/// delete them (plus their linked tracker rows, in a transaction) and return
/// the number deleted instead.
pub async fn clear_moods(
    pool: &SqlitePool,
    start_time: i64,
    end_time: i64,
    delete: bool,
) -> Result<usize> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood WHERE time >= ? AND time <= ?")
        .bind(start_time)
        .bind(end_time)
        .fetch_one(pool)
        .await
        .context("Failed to count mood entries")?;

    if !delete {
        return Ok(count as usize);
    }

    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    sqlx::query(
        "DELETE FROM tracker WHERE mood IN (SELECT id FROM mood WHERE time >= ? AND time <= ?)",
    )
    .bind(start_time)
    .bind(end_time)
    .execute(&mut *tx)
    .await
    .context("Failed to delete linked tracker entries")?;

    let res = sqlx::query("DELETE FROM mood WHERE time >= ? AND time <= ?")
        .bind(start_time)
        .bind(end_time)
        .execute(&mut *tx)
        .await
        .context("Failed to delete mood entries")?;

    tx.commit().await.context("Failed to commit transaction")?;
    Ok(res.rows_affected() as usize)
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

/// Moods in `[start, end]`, oldest first.
pub async fn fetch_moods_between(pool: &SqlitePool, start: i64, end: i64) -> Result<Vec<MoodRow>> {
    let rows = sqlx::query(
        "SELECT id, mood, body, time, embedding, score, duration, todo_id FROM mood WHERE time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch mood entries")?;

    Ok(rows
        .iter()
        .map(|row| MoodRow {
            id: row.get("id"),
            mood: row.get("mood"),
            body: row.get("body"),
            time: row.get("time"),
            embedding: row.get("embedding"),
            score: row.get("score"),
            duration: row.get("duration"),
            todo_id: row.get("todo_id"),
        })
        .collect())
}

/// Entries of one tracker in `[start, end]`, oldest first.
pub async fn fetch_tracker_entries(
    pool: &SqlitePool,
    tracker_type: &str,
    start: i64,
    end: i64,
) -> Result<Vec<TrackerEntryRow>> {
    let rows = sqlx::query(
        "SELECT id, type, CAST(score AS TEXT) AS score, time, mood FROM tracker WHERE type = ? AND time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(tracker_type)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch tracker entries")?;

    Ok(rows
        .iter()
        .map(|row| TrackerEntryRow {
            id: row.get("id"),
            tracker_type: row.get("type"),
            score: row.get("score"),
            time: row.get("time"),
            mood: row.get("mood"),
        })
        .collect())
}

/// For each tracker entry in `[start, end]`, the time of the previous
/// entry of the same type, keyed by entry id — the today-view preview's
/// `prev:` field. "Previous" is by time, with the row id as tiebreaker
/// (same-second entries: the one inserted first wins). Entries without an
/// earlier entry map to `None`.
pub async fn fetch_tracker_prev_times(
    pool: &SqlitePool,
    start: i64,
    end: i64,
) -> Result<std::collections::HashMap<i64, Option<i64>>> {
    let rows = sqlx::query(
        "SELECT t1.id, \
         (SELECT MAX(t2.time) FROM tracker t2 \
          WHERE t2.type = t1.type \
            AND (t2.time < t1.time OR (t2.time = t1.time AND t2.id < t1.id))) AS prev \
         FROM tracker t1 WHERE t1.time >= ? AND t1.time <= ?",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch tracker prev times")?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("id"), r.get::<Option<i64>, _>("prev")))
        .collect())
}

/// All tracker entries in `[start, end]`, oldest first (today view).
pub async fn fetch_tracker_entries_today(
    pool: &SqlitePool,
    start: i64,
    end: i64,
) -> Result<Vec<TrackerEntryRow>> {
    let rows = sqlx::query(
        "SELECT id, type, CAST(score AS TEXT) AS score, time, mood FROM tracker WHERE time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch today's tracker entries")?;

    Ok(rows
        .iter()
        .map(|row| TrackerEntryRow {
            id: row.get("id"),
            tracker_type: row.get("type"),
            score: row.get("score"),
            time: row.get("time"),
            mood: row.get("mood"),
        })
        .collect())
}

/// Recurring task metadata for the completion-dots tracker; accepts either a
/// numeric short id or the unique task name.
pub async fn fetch_recurring_task_meta(
    pool: &SqlitePool,
    name_or_id: &str,
) -> Result<Option<RecurringTaskMeta>> {
    let row = if let Ok(short_id) = name_or_id.parse::<i64>() {
        sqlx::query(
            "SELECT id, start_time, interval_secs, target_count FROM todos WHERE short_id = ? AND interval_secs IS NOT NULL",
        )
        .bind(short_id)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch recurring task")?
    } else {
        sqlx::query(
            "SELECT id, start_time, interval_secs, target_count FROM todos WHERE name = ? AND interval_secs IS NOT NULL",
        )
        .bind(name_or_id)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch recurring task")?
    };

    Ok(row.map(|r| RecurringTaskMeta {
        id: r.get("id"),
        start_time: r.get("start_time"),
        interval_secs: r.get("interval_secs"),
        target_count: r.get("target_count"),
    }))
}

/// Completion events (time, count) for a task in `[start, end]`.
pub async fn fetch_completions_between(
    pool: &SqlitePool,
    task_id: i64,
    start: i64,
    end: i64,
) -> Result<Vec<CompletionRow>> {
    let rows = sqlx::query(
        "SELECT time, count FROM todo_completions WHERE todo_id = ? AND time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(task_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch completion events")?;

    Ok(rows
        .iter()
        .map(|row| CompletionRow {
            time: row.get("time"),
            count: row.get("count"),
        })
        .collect())
}

/// Link a mood entry to a task (by stable row id). Since each mood can only
/// link to 1 task, any existing task link for this mood is replaced.
pub async fn link_mood_to_tasks(pool: &SqlitePool, mood_id: i64, task_ids: &[i64]) -> Result<()> {
    let task_id = task_ids.last().copied();
    sqlx::query("UPDATE mood SET todo_id = ? WHERE id = ?")
        .bind(task_id)
        .bind(mood_id)
        .execute(pool)
        .await
        .context("Failed to link mood to task")?;
    Ok(())
}

/// Link a mood entry to a task by the task's raw row id (the today-view
/// Link prompt): replaces any existing task link for the mood. A nonexistent task or mood id fails the
/// FK constraint — callers just log the result.
pub async fn link_mood_to_task(pool: &SqlitePool, mood_id: i64, task_id: i64) -> Result<u64> {
    let result = sqlx::query("UPDATE mood SET todo_id = ? WHERE id = ?")
        .bind(task_id)
        .bind(mood_id)
        .execute(pool)
        .await
        .context("Failed to link mood to task")?;
    Ok(result.rows_affected())
}

/// Attach a tracker entry to a mood row (the today-view Link prompt):
/// replaces the tracker's existing mood link (`tracker.mood`) or inserts
/// one when it had none. A nonexistent mood id fails the FK constraint.
pub async fn link_tracker_to_mood(pool: &SqlitePool, tracker_id: i64, mood_id: i64) -> Result<u64> {
    let result = sqlx::query("UPDATE tracker SET mood = ? WHERE id = ?")
        .bind(mood_id)
        .bind(tracker_id)
        .execute(pool)
        .await
        .context("Failed to link tracker entry to mood")?;
    Ok(result.rows_affected())
}

/// The mood entries linked to a task, oldest first (the task preview's
/// `moods:` field).
pub async fn fetch_linked_moods(pool: &SqlitePool, task_id: i64) -> Result<Vec<MoodRow>> {
    let rows = sqlx::query(
        "SELECT id, mood, body, time, embedding, score, duration, todo_id FROM mood \
         WHERE todo_id = ? ORDER BY time ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch linked moods")?;

    Ok(rows
        .iter()
        .map(|r| MoodRow {
            id: r.get("id"),
            mood: r.get("mood"),
            body: r.get("body"),
            time: r.get("time"),
            embedding: r.get("embedding"),
            score: r.get("score"),
            duration: r.get("duration"),
            todo_id: r.get("todo_id"),
        })
        .collect())
}

/// Mood rows by id (the today-view tracker preview's `mood:` field — the
/// mood a tracker entry is attached to). Absent ids are omitted from the
/// map; an empty input returns an empty map.
pub async fn fetch_moods_by_ids(
    pool: &SqlitePool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, MoodRow>> {
    let mut map = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT id, mood, body, time, embedding, score, duration, todo_id FROM mood WHERE id IN ({})",
        ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for id in ids {
        q = q.bind(id);
    }
    let rows = q
        .fetch_all(pool)
        .await
        .context("Failed to fetch moods by id")?;
    for row in rows {
        map.insert(
            row.get("id"),
            MoodRow {
                id: row.get("id"),
                mood: row.get("mood"),
                body: row.get("body"),
                time: row.get("time"),
                embedding: row.get("embedding"),
                score: row.get("score"),
                duration: row.get("duration"),
                todo_id: row.get("todo_id"),
            },
        );
    }
    Ok(map)
}

/// Tracker entries attached to moods (the `tracker.mood` column),
/// grouped by mood id, oldest first within each group. Moods without
/// attached tracker rows are absent from the map; an empty input returns an
/// empty map.
pub async fn fetch_mood_trackers(
    pool: &SqlitePool,
    mood_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<TrackerEntryRow>>> {
    let mut map = std::collections::HashMap::new();
    if mood_ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT id, type, CAST(score AS TEXT) AS score, time, mood FROM tracker \
         WHERE mood IN ({}) ORDER BY time ASC",
        mood_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for id in mood_ids {
        q = q.bind(id);
    }
    let rows = q
        .fetch_all(pool)
        .await
        .context("Failed to fetch tracker rows linked to moods")?;
    for row in rows {
        let entry = TrackerEntryRow {
            id: row.get("id"),
            tracker_type: row.get("type"),
            score: row.get("score"),
            time: row.get("time"),
            mood: row.get("mood"),
        };
        map.entry(row.get::<i64, _>("mood"))
            .or_insert_with(Vec::new)
            .push(entry);
    }
    Ok(map)
}

/// Tasks linked to moods via `mood.todo_id`, grouped by mood id,
/// ordered by name. Completions/last_time follow the today-view convention
/// (full completion scoping via [`attach_full_completions`]). An empty
/// input returns an empty map.
pub async fn fetch_mood_tasks(
    pool: &SqlitePool,
    mood_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<TaskRow>>> {
    let mut map = std::collections::HashMap::new();
    if mood_ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT t.*, m.id AS mood_id, NULL AS completions, NULL AS last_time \
         FROM todos t JOIN mood m ON m.todo_id = t.id \
         WHERE m.id IN ({}) ORDER BY t.name ASC",
        mood_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for id in mood_ids {
        q = q.bind(id);
    }
    let rows = q
        .fetch_all(pool)
        .await
        .context("Failed to fetch tasks linked to moods")?;
    // Reconstruct a TaskRow per link row (the query carries the extra
    // `mood_id` column, which query_as::<TaskRow> would drop), then
    // attach the completion aggregates to the unique tasks.
    let mut links: Vec<(i64, i64)> = Vec::new();
    let mut tasks: Vec<TaskRow> = Vec::new();
    for row in rows {
        links.push((row.get("mood_id"), row.get("id")));
        tasks.push(TaskRow {
            id: row.get("id"),
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
    }
    let by_id: std::collections::HashMap<i64, TaskRow> =
        attach_full_completions(pool, tasks, crate::date::now())
            .await?
            .into_iter()
            .map(|t| (t.id, t))
            .collect();
    for (mood_id, task_id) in links {
        if let Some(task) = by_id.get(&task_id) {
            map.entry(mood_id)
                .or_insert_with(Vec::new)
                .push(task.clone());
        }
    }
    Ok(map)
}

/// Delete a tracker entry row.
pub async fn delete_tracker_entry(pool: &SqlitePool, id: i64) -> Result<u64> {
    let result = sqlx::query("DELETE FROM tracker WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to delete tracker row")?;
    Ok(result.rows_affected())
}

/// One deletion rule for `:db doctor`, computed from the tracker's current
/// configured kind. Rules are applied in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerPruneRule {
    /// Keep only entries whose SQLite storage class equals `keep`; delete
    /// the rest. `keep` is `text` for kind text, `integer` for
    /// integer/null, `real` for float/duration — the storage class every
    /// writer binds for that kind (`create_entry`, `update_tracker_score`).
    Storage {
        tracker_type: String,
        keep: &'static str,
    },
    /// Delete every entry with `score != 0` (any storage class). Time-marker
    /// null trackers — `null` with both min and max set — always write
    /// score 0, so nonzero rows are stale count-mode leftovers.
    NonzeroScore { tracker_type: String },
    /// Delete every entry of a type with no `[tracker.<type>]` section in
    /// the config (renamed/removed tracker; the today view errors on such
    /// rows).
    All { tracker_type: String },
}

/// Storage-class distribution of tracker entries, grouped by type and
/// `typeof(score)`, for `:db doctor`. `nonzero` counts `score != 0` rows
/// within the bucket (COALESCE'd; only meaningful for integer buckets).
pub async fn fetch_tracker_score_kinds(pool: &SqlitePool) -> Result<Vec<TrackerScoreKindRow>> {
    let rows = sqlx::query(
        "SELECT type, typeof(score) AS storage, COUNT(*) AS count, \
         COALESCE(SUM(CASE WHEN score != 0 THEN 1 ELSE 0 END), 0) AS nonzero \
         FROM tracker GROUP BY type, typeof(score) ORDER BY type, storage",
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch tracker score kinds")?;

    Ok(rows
        .iter()
        .map(|r| TrackerScoreKindRow {
            tracker_type: r.get("type"),
            storage: r.get("storage"),
            count: r.get("count"),
            nonzero: r.get("nonzero"),
        })
        .collect())
}

/// Apply `:db doctor` prune rules in one transaction; returns the total
/// number of rows deleted. `NonzeroScore` and `All` may overlap a `Storage`
/// rule's rows, but each rule deletes only rows still present, so the
/// per-rule `rows_affected` counts are disjoint.
pub async fn prune_tracker_rules(pool: &SqlitePool, rules: &[TrackerPruneRule]) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let mut deleted = 0u64;
    for rule in rules {
        let res = match rule {
            TrackerPruneRule::Storage { tracker_type, keep } => {
                sqlx::query("DELETE FROM tracker WHERE type = ? AND typeof(score) != ?")
                    .bind(tracker_type)
                    .bind(keep)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to prune mismatched entries for tracker '{tracker_type}'")
                    })?
            }
            TrackerPruneRule::NonzeroScore { tracker_type } => {
                sqlx::query("DELETE FROM tracker WHERE type = ? AND score != 0")
                    .bind(tracker_type)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to prune nonzero entries for tracker '{tracker_type}'")
                    })?
            }
            TrackerPruneRule::All { tracker_type } => {
                sqlx::query("DELETE FROM tracker WHERE type = ?")
                    .bind(tracker_type)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to prune orphan entries for tracker '{tracker_type}'")
                    })?
            }
        };
        deleted += res.rows_affected();
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(deleted)
}

/// Update a mood's body. Returns the number of affected rows.
pub async fn update_mood_body(pool: &SqlitePool, id: i64, body: &str) -> Result<u64> {
    let res = sqlx::query("UPDATE mood SET body = ? WHERE id = ?")
        .bind(body)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update mood body")?;
    Ok(res.rows_affected())
}

/// Update one tracker entry's score in place. `value` must already be
/// validated for the tracker's kind (callers run the strict pipeline —
/// [`crate::tracker::parse_tracker_value`] then
/// [`crate::tracker::enforce_strict`]), so the value variant alone decides
/// the bound storage class. Returns affected rows.
pub async fn update_tracker_score(pool: &SqlitePool, id: i64, value: &TrackerValue) -> Result<u64> {
    let mut q = sqlx::query("UPDATE tracker SET score = ? WHERE id = ?");
    q = match value {
        TrackerValue::Text(s) => q.bind(s.as_str()),
        TrackerValue::Integer(n) => q.bind(*n),
        TrackerValue::Float(f) => q.bind(*f),
    };
    let res = q
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update tracker score")?;
    Ok(res.rows_affected())
}

/// The current timestamp of a tracker entry (for the TUI update action's
/// cross-slot check).
pub async fn fetch_tracker_time(pool: &SqlitePool, id: i64) -> Result<Option<i64>> {
    sqlx::query_scalar("SELECT time FROM tracker WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch tracker entry time")
}

/// Move a tracker entry's timestamp to `time`. Used by the TUI update
/// action on null tracker rows: timestamp only — no score change, no
/// deletes. The caller checks that the move stays within the row's current
/// interval slot. Returns affected rows.
pub async fn update_tracker_time(pool: &SqlitePool, id: i64, time: i64) -> Result<u64> {
    let res = sqlx::query("UPDATE tracker SET time = ? WHERE id = ?")
        .bind(time)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to update tracker entry time")?;
    Ok(res.rows_affected())
}

/// Delete a mood row and any linked tracker rows in a transaction
/// (`tracker.mood` has a FK with no `ON DELETE CASCADE`).
pub async fn delete_mood(pool: &SqlitePool, id: i64) -> Result<()> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    sqlx::query("DELETE FROM tracker WHERE mood = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete linked tracker rows")?;

    sqlx::query("DELETE FROM mood WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete mood row")?;

    tx.commit().await.context("Failed to commit transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use crate::db::{
        EntryObject, TaskObject, TrackerObject, TrackerValue, create_entry, create_task,
    };

    /// Seed a mood row; returns its id.
    async fn seed_mood(pool: &SqlitePool, mood: &str) -> i64 {
        create_entry(
            pool,
            &EntryObject {
                mood: mood.to_string(),
                body: String::new(),
                time: 1_700_000_000,
                embedding: None,
                score: None,
                trackers: Vec::new(),
                duration: None,
                todo_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap()
    }

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
    async fn test_link_mood_to_task() {
        let pool = test_pool().await.unwrap();
        let mood_id = seed_mood(&pool, "good").await;
        let task_id = seed_task(&pool, "t").await;

        let affected = link_mood_to_task(&pool, mood_id, task_id).await.unwrap();
        assert_eq!(affected, 1);
        let linked = fetch_linked_moods(&pool, task_id).await.unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, mood_id);

        // Re-linking replaces the link:
        let task_id2 = seed_task(&pool, "t2").await;
        let affected = link_mood_to_task(&pool, mood_id, task_id2).await.unwrap();
        assert_eq!(affected, 1);
        assert_eq!(fetch_linked_moods(&pool, task_id).await.unwrap().len(), 0);
        assert_eq!(fetch_linked_moods(&pool, task_id2).await.unwrap().len(), 1);

        // Nonexistent task id fails the FK constraint.
        assert!(link_mood_to_task(&pool, mood_id, 9999).await.is_err());
    }

    #[tokio::test]
    async fn test_link_tracker_to_mood() {
        let pool = test_pool().await.unwrap();
        // A tracker-only entry (no mood row inserted): `mood` is NULL.
        assert!(
            create_entry(
                &pool,
                &EntryObject {
                    mood: String::new(),
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
            .is_none()
        );
        let tracker_id: i64 = sqlx::query_scalar("SELECT id FROM tracker ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let mood_a = seed_mood(&pool, "good").await;
        let mood_b = seed_mood(&pool, "bad").await;

        // Insert the link (the tracker had none).
        let affected = link_tracker_to_mood(&pool, tracker_id, mood_a)
            .await
            .unwrap();
        assert_eq!(affected, 1);
        let mood: Option<i64> = sqlx::query_scalar("SELECT mood FROM tracker WHERE id = ?")
            .bind(tracker_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mood, Some(mood_a));

        // Re-link: replaces the existing attachment.
        let affected = link_tracker_to_mood(&pool, tracker_id, mood_b)
            .await
            .unwrap();
        assert_eq!(affected, 1);
        let mood: Option<i64> = sqlx::query_scalar("SELECT mood FROM tracker WHERE id = ?")
            .bind(tracker_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mood, Some(mood_b));

        // Nonexistent tracker id → 0 rows; nonexistent mood id fails the FK.
        assert_eq!(link_tracker_to_mood(&pool, 9999, mood_b).await.unwrap(), 0);
        assert!(link_tracker_to_mood(&pool, tracker_id, 9999).await.is_err());
    }
}
