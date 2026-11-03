use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use super::Id;
use super::models::{
    CompletionRow, EntryObject, MoodRow, RecurringTaskMeta, TaskRow, TrackerEntryRow,
    TrackerScoreKindRow, TrackerValue, log_offset, log_time, mood_row, tracker_entry_row,
};
use super::views::attach_full_completions;
use crate::date::CivilRange;

/// Raw-fetch margin for rebased tracker membership: the widest possible
/// difference between a recorded and a current UTC offset is under two days.
pub const CIVIL_FETCH_MARGIN: i64 = 2 * 86_400;

/// Insert a mood entry and its linked tracker values in one transaction.
/// For Text/Float interval trackers, `replace_slot` deletes the previous
/// entry in the same interval slot before inserting. Returns the mood
/// row id, or `None` when no mood row was inserted (tracker-only entry).
/// Every row written or removed gets its sync event inside the transaction.
pub async fn create_entry(pool: &SqlitePool, entry: &EntryObject) -> Result<Option<Id>> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    let insert_mood = !entry.mood.is_empty()
        || !entry.body.is_empty()
        || entry.duration.is_some()
        || entry.todo_id.is_some();
    let mood_id: Option<Id> = if insert_mood {
        let id = Id::new();
        if let Some(blob) = &entry.embedding {
            sqlx::query(
                "INSERT INTO mood (id, mood, body, time, tz_offset, location, embedding, score, duration, todo_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&entry.mood)
            .bind(&entry.body)
            .bind(entry.time.epoch)
            .bind(entry.time.offset)
            .bind(&entry.location)
            .bind(blob)
            .bind(entry.score)
            .bind(entry.duration)
            .bind(entry.todo_id)
            .execute(&mut *tx)
            .await
            .context("Failed to insert mood")?;
        } else {
            sqlx::query(
                "INSERT INTO mood (id, mood, body, time, tz_offset, location, score, duration, todo_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&entry.mood)
            .bind(&entry.body)
            .bind(entry.time.epoch)
            .bind(entry.time.offset)
            .bind(&entry.location)
            .bind(entry.score)
            .bind(entry.duration)
            .bind(entry.todo_id)
            .execute(&mut *tx)
            .await
            .context("Failed to insert mood")?;
        }
        crate::sync::events::mood_create(&mut tx, id).await?;
        Some(id)
    } else {
        None
    };

    for tracker in &entry.trackers {
        if let Some((slot_start, slot_end)) = tracker.replace_slot {
            let candidates = sqlx::query(
                "SELECT id, time, tz_offset FROM tracker \
                 WHERE type = ? AND time >= ? AND time < ?",
            )
            .bind(&tracker.tracker_type)
            .bind(slot_start - CIVIL_FETCH_MARGIN)
            .bind(slot_end + CIVIL_FETCH_MARGIN)
            .fetch_all(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "Failed to read the replaced entries of tracker '{}' in slot {}..{}",
                    tracker.tracker_type, slot_start, slot_end
                )
            })?;
            let replaced: Vec<Id> = candidates
                .iter()
                .filter(|row| {
                    let stamp = log_time(row);
                    let rebased = stamp.rebased();
                    rebased >= slot_start && rebased < slot_end
                })
                .map(|row| row.get::<Id, _>("id"))
                .collect();
            for id in &replaced {
                sqlx::query("DELETE FROM tracker WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("Failed to delete replaced tracker row {id:?}"))?;
                crate::sync::events::delete(&mut tx, *id, "tracker").await?;
            }
        }

        let tracker_id = Id::new();
        let mut q = sqlx::query(
            "INSERT INTO tracker (id, type, score, time, tz_offset, location, mood) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(tracker_id)
        .bind(&tracker.tracker_type);
        q = match &tracker.value {
            TrackerValue::Text(s) => q.bind(s),
            TrackerValue::Integer(n) => q.bind(n),
            TrackerValue::Float(f) => q.bind(f),
        };
        q.bind(entry.time.epoch)
            .bind(entry.time.offset)
            .bind(&entry.location)
            .bind(mood_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to insert tracker '{}'", tracker.tracker_type))?;
        crate::sync::events::tracker_create(&mut tx, tracker_id).await?;
    }

    tx.commit().await.context("Failed to commit transaction")?;
    Ok(mood_id)
}

/// Count mood entries in `[start_time, end_time]`; when `delete` is true,
/// delete them (plus their linked tracker rows, in a transaction) and return
/// the number deleted instead. Every removed row gets a delete event.
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

    let removed_trackers: Vec<Id> = sqlx::query_scalar(
        "SELECT id FROM tracker WHERE mood IN (SELECT id FROM mood WHERE time >= ? AND time <= ?)",
    )
    .bind(start_time)
    .bind(end_time)
    .fetch_all(&mut *tx)
    .await
    .context("Failed to read linked tracker entries")?;
    let removed_moods: Vec<Id> =
        sqlx::query_scalar("SELECT id FROM mood WHERE time >= ? AND time <= ?")
            .bind(start_time)
            .bind(end_time)
            .fetch_all(&mut *tx)
            .await
            .context("Failed to read mood entries")?;
    // Publish before the rows go: a delete carries a snapshot of what it
    // removed, and that snapshot is only readable while the row exists.
    for id in &removed_trackers {
        crate::sync::events::delete(&mut tx, *id, "tracker").await?;
    }
    for id in &removed_moods {
        crate::sync::events::delete(&mut tx, *id, "mood").await?;
    }
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
        "SELECT id, mood, body, time, tz_offset, location, embedding, score, duration, todo_id FROM mood WHERE time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch mood entries")?;

    Ok(rows
        .iter()
        .map(mood_row)
        .collect())
}

/// Moods whose recorded civil time falls in `range`, oldest first.
pub async fn fetch_moods_civil(pool: &SqlitePool, range: CivilRange) -> Result<Vec<MoodRow>> {
    let rows = sqlx::query(
        "SELECT id, mood, body, time, tz_offset, location, embedding, score, duration, todo_id FROM mood \
         WHERE time + tz_offset BETWEEN ? AND ? ORDER BY time ASC",
    )
    .bind(range.start)
    .bind(range.end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch mood entries by civil time")?;

    Ok(rows
        .iter()
        .map(mood_row)
        .collect())
}

/// All tracker entries whose recorded civil time falls in `range` (today view).
pub async fn fetch_tracker_entries_today_civil(
    pool: &SqlitePool,
    range: CivilRange,
) -> Result<Vec<TrackerEntryRow>> {
    let rows = sqlx::query(
        "SELECT id, type, CAST(score AS TEXT) AS score, time, tz_offset, location, mood FROM tracker \
         WHERE time + tz_offset BETWEEN ? AND ? ORDER BY time ASC",
    )
    .bind(range.start)
    .bind(range.end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch today's tracker entries by civil time")?;
    Ok(rows
        .iter()
        .map(tracker_entry_row)
        .collect())
}

/// The previous entry of the same type for every tracker row whose civil
/// time falls in `range`, keyed by the current row id — the today-view
/// preview's `prev:` field. "Previous" is by time, with the row id as
/// tiebreaker (same-second entries: the one inserted first wins); the
/// predecessor may lie outside `range`. Its own stamp and location are
/// returned, not the current row's.
pub async fn fetch_tracker_prev_origins(
    pool: &SqlitePool,
    range: CivilRange,
) -> Result<std::collections::HashMap<Id, Option<crate::date::LogOrigin>>> {
    let rows = sqlx::query(
        "SELECT t1.id AS id, t2.time AS prev_time, \
         t2.tz_offset AS prev_tz_offset, t2.location AS prev_location \
         FROM tracker t1 \
         LEFT JOIN tracker t2 ON t2.id = (\
             SELECT t3.id FROM tracker t3 \
             WHERE t3.type = t1.type \
               AND (t3.time < t1.time OR (t3.time = t1.time AND t3.id < t1.id)) \
             ORDER BY t3.time DESC, t3.id DESC LIMIT 1) \
         WHERE t1.time + t1.tz_offset BETWEEN ? AND ?",
    )
    .bind(range.start)
    .bind(range.end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch tracker prev origins by civil time")?;
    Ok(rows
        .iter()
        .map(|r| {
            let prev = r.get::<Option<i64>, _>("prev_time").map(|epoch| {
                crate::date::LogOrigin {
                    time: crate::date::LogTime::new(epoch, log_offset(r, "prev_tz_offset")),
                    location: r.get("prev_location"),
                }
            });
            (r.get::<Id, _>("id"), prev)
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
        "SELECT id, type, CAST(score AS TEXT) AS score, time, tz_offset, location, mood FROM tracker WHERE type = ? AND time >= ? AND time <= ? ORDER BY time ASC",
    )
    .bind(tracker_type)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch tracker entries")?;

    Ok(rows
        .iter()
        .map(tracker_entry_row)
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
    task_id: Id,
    start: i64,
    end: i64,
) -> Result<Vec<CompletionRow>> {
    let rows = sqlx::query(
        "SELECT time, tz_offset, location, count FROM todo_completions \
         WHERE todo_id = ? AND time >= ? AND time <= ? \
         ORDER BY time ASC",
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
            time: log_time(row),
            location: row.get("location"),
            count: row.get("count"),
        })
        .collect())
}

/// Link a mood entry to a task (by stable row id). Since each mood can only
/// link to 1 task, any existing task link for this mood is replaced.
pub async fn link_mood_to_tasks(pool: &SqlitePool, mood_id: Id, task_ids: &[Id]) -> Result<()> {
    let task_id = task_ids.last().copied();
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let res = sqlx::query("UPDATE mood SET todo_id = ? WHERE id = ?")
        .bind(task_id)
        .bind(mood_id)
        .execute(&mut *tx)
        .await
        .context("Failed to link mood to task")?;
    if res.rows_affected() > 0 {
        crate::sync::events::mood_update(
            &mut tx,
            mood_id,
            crate::sync::MoodUpdate::TodoId(task_id),
        )
        .await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(())
}

/// Link a mood entry to a task by the task's raw row id (the today-view
/// Link prompt): replaces any existing task link for the mood. A nonexistent task or mood id fails the
/// FK constraint — callers just log the result.
pub async fn link_mood_to_task(pool: &SqlitePool, mood_id: Id, task_id: Id) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let result = sqlx::query("UPDATE mood SET todo_id = ? WHERE id = ?")
        .bind(task_id)
        .bind(mood_id)
        .execute(&mut *tx)
        .await
        .context("Failed to link mood to task")?;
    if result.rows_affected() > 0 {
        crate::sync::events::mood_update(
            &mut tx,
            mood_id,
            crate::sync::MoodUpdate::TodoId(Some(task_id)),
        )
        .await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(result.rows_affected())
}

/// Attach a tracker entry to a mood row (the today-view Link prompt):
/// replaces the tracker's existing mood link (`tracker.mood`) or inserts
/// one when it had none. A nonexistent mood id fails the FK constraint.
pub async fn link_tracker_to_mood(pool: &SqlitePool, tracker_id: Id, mood_id: Id) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let result = sqlx::query("UPDATE tracker SET mood = ? WHERE id = ?")
        .bind(mood_id)
        .bind(tracker_id)
        .execute(&mut *tx)
        .await
        .context("Failed to link tracker entry to mood")?;
    if result.rows_affected() > 0 {
        crate::sync::events::tracker_update(
            &mut tx,
            tracker_id,
            crate::sync::TrackerUpdate::MoodId(Some(mood_id)),
        )
        .await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(result.rows_affected())
}

/// The mood entries linked to a task, oldest first (the task preview's
/// `moods:` field).
pub async fn fetch_linked_moods(pool: &SqlitePool, task_id: Id) -> Result<Vec<MoodRow>> {
    let rows = sqlx::query(
        "SELECT id, mood, body, time, tz_offset, location, embedding, score, duration, todo_id FROM mood \
         WHERE todo_id = ? ORDER BY time ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch linked moods")?;

    Ok(rows
        .iter()
        .map(mood_row)
        .collect())
}

/// Mood rows by id (the today-view tracker preview's `mood:` field — the
/// mood a tracker entry is attached to). Absent ids are omitted from the
/// map; an empty input returns an empty map.
pub async fn fetch_moods_by_ids(
    pool: &SqlitePool,
    ids: &[Id],
) -> Result<std::collections::HashMap<Id, MoodRow>> {
    let mut map = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT id, mood, body, time, tz_offset, location, embedding, score, duration, todo_id FROM mood WHERE id IN ({})",
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
            row.get::<Id, _>("id"),
            mood_row(&row),
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
    mood_ids: &[Id],
) -> Result<std::collections::HashMap<Id, Vec<TrackerEntryRow>>> {
    let mut map = std::collections::HashMap::new();
    if mood_ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT id, type, CAST(score AS TEXT) AS score, time, tz_offset, location, mood FROM tracker \
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
        let entry = tracker_entry_row(&row);
        map.entry(row.get::<Id, _>("mood"))
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
    mood_ids: &[Id],
) -> Result<std::collections::HashMap<Id, Vec<TaskRow>>> {
    let mut map = std::collections::HashMap::new();
    if mood_ids.is_empty() {
        return Ok(map);
    }
    let sql = format!(
        "SELECT t.*, m.id AS mood_id, NULL AS completions, NULL AS last_time, NULL AS last_tz_offset, '' AS last_location \
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
    let mut links: Vec<(Id, Id)> = Vec::new();
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
            last_location: String::new(),
        });
    }
    let by_id: std::collections::HashMap<Id, TaskRow> =
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
pub async fn delete_tracker_entry(pool: &SqlitePool, id: Id) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let result = sqlx::query("DELETE FROM tracker WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete tracker row")?;
    if result.rows_affected() > 0 {
        crate::sync::events::delete(&mut tx, id, "tracker").await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
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
    /// Delete every entry of a type with no `[tracker.<type>]` section in
    /// the config (renamed/removed tracker; the today view errors on such
    /// rows).
    All { tracker_type: String },
}

/// Storage-class distribution of tracker entries, grouped by type and
/// `typeof(score)`, for `:db doctor`. `nonzero` counts `score != 0` rows
/// within the bucket (COALESCE'd; informational for integer buckets).
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
/// number of rows deleted. `All` may overlap a `Storage` rule's rows, but
/// each rule deletes only rows still present, so the per-rule
/// `rows_affected` counts are disjoint. Every removed row gets a delete
/// event.
pub async fn prune_tracker_rules(pool: &SqlitePool, rules: &[TrackerPruneRule]) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let mut deleted = 0u64;
    for rule in rules {
        let (predicate, bind_type): (&str, Option<&str>) = match rule {
            TrackerPruneRule::Storage {
                tracker_type,
                keep: _,
            } => ("type = ? AND typeof(score) != ?", Some(tracker_type)),
            TrackerPruneRule::All { tracker_type } => ("type = ?", Some(tracker_type)),
        };
        let select = format!("SELECT id FROM tracker WHERE {predicate}");
        let mut query = sqlx::query_scalar::<_, Id>(sqlx::AssertSqlSafe(select));
        query = query.bind(bind_type);
        if let TrackerPruneRule::Storage { keep, .. } = rule {
            query = query.bind(keep);
        }
        let removed: Vec<Id> = query
            .fetch_all(&mut *tx)
            .await
            .with_context(|| format!("Failed to read pruned entries for rule '{rule:?}'"))?;

        let delete = format!("DELETE FROM tracker WHERE {predicate}");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(delete));
        query = query.bind(bind_type);
        if let TrackerPruneRule::Storage { keep, .. } = rule {
            query = query.bind(keep);
        }
        let res = query
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to prune entries for rule '{rule:?}'"))?;
        for id in &removed {
            crate::sync::events::delete(&mut tx, *id, "tracker").await?;
        }
        deleted += res.rows_affected();
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(deleted)
}

/// Update a mood's body. Returns the number of affected rows.
pub async fn update_mood_body(pool: &SqlitePool, id: Id, body: &str) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let res = sqlx::query("UPDATE mood SET body = ? WHERE id = ?")
        .bind(body)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to update mood body")?;
    if res.rows_affected() > 0 {
        crate::sync::events::mood_update(
            &mut tx,
            id,
            crate::sync::MoodUpdate::Body(body.to_string()),
        )
        .await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(res.rows_affected())
}

/// Update one tracker entry's score in place. `value` must already be
/// validated for the tracker's kind (callers run the strict pipeline —
/// [`crate::tracker::parse_tracker_value`] then
/// [`crate::tracker::enforce_strict`]), so the value variant alone decides
/// the bound storage class. Returns affected rows.
pub async fn update_tracker_score(pool: &SqlitePool, id: Id, value: &TrackerValue) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let mut q = sqlx::query("UPDATE tracker SET score = ? WHERE id = ?");
    q = match value {
        TrackerValue::Text(s) => q.bind(s.as_str()),
        TrackerValue::Integer(n) => q.bind(*n),
        TrackerValue::Float(f) => q.bind(*f),
    };
    let res = q
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to update tracker score")?;
    if res.rows_affected() > 0 {
        crate::sync::events::tracker_update(
            &mut tx,
            id,
            crate::sync::TrackerUpdate::Score(crate::sync::CellValue::from(value)),
        )
        .await?;
    }
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(res.rows_affected())
}

/// The current timestamp of a tracker entry (for the TUI update action's
/// cross-slot check).
pub async fn fetch_tracker_time(pool: &SqlitePool, id: Id) -> Result<Option<crate::date::LogTime>> {
    let row = sqlx::query("SELECT time, tz_offset FROM tracker WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch tracker entry time")?;
    Ok(row.map(|row| log_time(&row)))
}

/// Move a tracker entry to `time` by replacing it.
///
/// `time` decides which slot a row occupies, so it is fixed when the row is
/// created: a move publishes a delete and an ordinary new row rather than
/// rewriting the field that decided the old slot. The caller checks that the
/// move stays within the row's current interval slot. `score_override`
/// rewrites a replace-mode null marker's stored seconds-from-start position;
/// the re-created row carries the current zone's offset and name.
pub async fn update_tracker_time(
    pool: &SqlitePool,
    id: Id,
    time: crate::date::LogTime,
    score_override: Option<i64>,
) -> Result<u64> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let row = sqlx::query(
        "SELECT type, typeof(score) AS score_kind, CAST(score AS TEXT) AS score_text, mood
           FROM tracker WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .context("Failed to read the tracker entry")?;
    let Some(row) = row else { return Ok(0) };
    let tracker_type: String = row.get("type");
    let score = match score_override {
        Some(value) => TrackerValue::Integer(value),
        None => {
            crate::db::tracker_value(row.get("score_kind"), &row.get::<String, _>("score_text"))
        }
    };
    let location = crate::date::current_location();
    let mood: Option<Id> = row.get("mood");

    sqlx::query("DELETE FROM tracker WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to remove the replaced tracker entry")?;
    crate::sync::events::delete(&mut tx, id, "tracker").await?;

    let replacement = Id::new();
    let mut insert = sqlx::query(
        "INSERT INTO tracker (id, type, score, time, tz_offset, location, mood) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(replacement)
    .bind(&tracker_type);
    insert = match &score {
        TrackerValue::Text(text) => insert.bind(text),
        TrackerValue::Integer(number) => insert.bind(number),
        TrackerValue::Float(number) => insert.bind(number),
    };
    insert
        .bind(time.epoch)
        .bind(time.offset)
        .bind(location)
        .bind(mood)
        .execute(&mut *tx)
        .await
        .context("Failed to insert the moved tracker entry")?;
    crate::sync::events::tracker_create(&mut tx, replacement).await?;
    tx.commit().await.context("Failed to commit transaction")?;
    Ok(1)
}

/// Delete a mood row and any linked tracker rows in a transaction
/// (`tracker.mood` is `ON DELETE SET NULL`, so the rows are removed here).
pub async fn delete_mood(pool: &SqlitePool, id: Id) -> Result<()> {
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    let linked: Vec<Id> = sqlx::query_scalar("SELECT id FROM tracker WHERE mood = ?")
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .context("Failed to read linked tracker rows")?;
    sqlx::query("DELETE FROM tracker WHERE mood = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete linked tracker rows")?;
    for tracker in &linked {
        crate::sync::events::delete(&mut tx, *tracker, "tracker").await?;
    }

    let res = sqlx::query("DELETE FROM mood WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete mood row")?;
    if res.rows_affected() > 0 {
        crate::sync::events::delete(&mut tx, id, "mood").await?;
    }

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
    async fn seed_mood(pool: &SqlitePool, mood: &str) -> Id {
        create_entry(
            pool,
            &EntryObject {
                mood: mood.to_string(),
                body: String::new(),
                time: crate::date::LogTime::local(1_700_000_000),
                location: String::new(),
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
    async fn seed_task(pool: &SqlitePool, name: &str) -> Id {
        let (id, _) = create_task(
            pool,
            &TaskObject {
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
        assert!(link_mood_to_task(&pool, mood_id, Id::new()).await.is_err());
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
                    time: crate::date::LogTime::local(1_700_000_000),
                    location: String::new(),
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
        let tracker_id: Id = sqlx::query_scalar("SELECT id FROM tracker ORDER BY id DESC LIMIT 1")
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
        let mood: Option<Id> = sqlx::query_scalar("SELECT mood FROM tracker WHERE id = ?")
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
        let mood: Option<Id> = sqlx::query_scalar("SELECT mood FROM tracker WHERE id = ?")
            .bind(tracker_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mood, Some(mood_b));

        // Nonexistent tracker id → 0 rows; nonexistent mood id fails the FK.
        assert_eq!(
            link_tracker_to_mood(&pool, Id::new(), mood_b)
                .await
                .unwrap(),
            0
        );
        assert!(
            link_tracker_to_mood(&pool, tracker_id, Id::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn create_entry_emits_location_cells() {
        let pool = crate::db::test_pool().await.unwrap();
        create_entry(
            &pool,
            &EntryObject {
                mood: "zoned".to_string(),
                body: String::new(),
                time: crate::date::LogTime::new(1_700_000_000, -18_000),
                location: "America/New_York".to_string(),
                embedding: None,
                score: None,
                trackers: vec![TrackerObject {
                    tracker_type: "water".to_string(),
                    value: TrackerValue::Integer(1),
                    replace_slot: None,
                }],
                duration: None,
                todo_id: None,
            },
        )
        .await
        .unwrap();

        for kind in ["mood", "tracker"] {
            let offsets: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM _sync_events WHERE kind = ? AND field = 'tz_offset'",
            )
            .bind(kind)
            .fetch_one(&pool)
            .await
            .unwrap();
            let locations: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM _sync_events WHERE kind = ? AND field = 'location'",
            )
            .bind(kind)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(offsets, 1, "{kind} carries its offset");
            assert_eq!(locations, 1, "{kind} carries its location");
        }
    }
}
