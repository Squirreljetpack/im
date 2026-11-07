use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use super::Id;
use crate::date::LogTime;
use crate::types::TaskKind;

/// A task as seen by the creation/edit flows. `id` is the stable row id
/// (`Some` for existing tasks; `None` for new tasks — the row id is
/// autoassigned at insert time). `short_id` is the user-facing id: always
/// `None` on new tasks (the SQL layer allocates it), and `None` for
/// existing oneshot tasks once they are completed.
#[derive(Debug, Clone)]
pub struct TaskObject {
    pub id: Option<Id>,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: i64,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`] (see
    /// [`crate::date::span_to_db`]) — NOT seconds despite the name.
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: bool,
    pub end_time: Option<i64>,
    /// Parent task id (task tree); `None` for root-level tasks. Not
    /// settable through the CLI yet — creation always inserts root tasks.
    pub parent: Option<Id>,
}

impl TaskObject {
    pub fn is_recurring(&self) -> bool {
        self.interval_secs.is_some()
    }

    /// The recurrence interval as a calendar `jiff::Span`.
    pub fn interval_span(&self) -> Option<jiff::Span> {
        self.interval_secs.map(crate::date::db_to_span)
    }

    /// A scheduled task: no recurrence interval, with an availability
    /// window. See [`TaskRow::is_scheduled`].
    pub fn is_scheduled(&self) -> bool {
        self.interval_secs.is_none() && self.available_duration_secs.is_some()
    }
}

/// The task fields editable via the interactive edit flow.
#[derive(Debug, Clone)]
pub struct UpdateTaskObject {
    pub id: Id,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: i64,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`].
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: bool,
    pub end_time: Option<i64>,
    pub parent: Option<Id>,
}

/// A logged mood entry plus any linked tracker values.
///
/// `trackers` carries the pre-resolved `TrackerValue`s and, for interval
/// trackers in replace mode, the slot `(start, end)` whose previous entries
/// are deleted inside the insert transaction.
#[derive(Debug, Clone)]
pub struct EntryObject {
    pub mood: String,
    pub body: String,
    pub time: LogTime,
    /// Location string committed at write time (empty when the system had no name).
    pub location: String,
    pub embedding: Option<Vec<u8>>,
    /// Cached emotional-saliency score for the mood text, computed at entry
    /// creation (`None` for journal-only rows or failed embeddings).
    pub score: Option<f32>,
    pub trackers: Vec<TrackerObject>,
    pub duration: Option<i64>,
    pub todo_id: Option<Id>,
}

#[derive(Debug, Clone)]
pub struct TrackerObject {
    pub tracker_type: String,
    pub value: TrackerValue,
    /// `[start, end)` interval slot whose previous entries are deleted
    /// before the insert (replace mode: every kind with an interval).
    pub replace_slot: Option<(i64, i64)>,
}

/// Typed payload of a tracker entry, determined by its configured kind.
/// `Duration` values are stored as `Float` seconds — display sites key on
/// `TrackerKind`, not on the value variant.
#[derive(Debug, Clone)]
pub enum TrackerValue {
    Text(String),
    Integer(i64),
    Float(f64),
}

/// Rebuild a tracker value from its storage class and text form: the column
/// is dynamically typed, so `typeof(score)` plus `CAST(score AS TEXT)` is the
/// only lossless way to read it back.
pub fn tracker_value(storage_class: &str, text: &str) -> TrackerValue {
    match storage_class {
        "integer" => TrackerValue::Integer(text.parse().unwrap_or_default()),
        "real" => TrackerValue::Float(text.parse().unwrap_or_default()),
        _ => TrackerValue::Text(text.to_string()),
    }
}

impl std::fmt::Display for TrackerValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackerValue::Text(s) => write!(f, "{}", s),
            TrackerValue::Integer(n) => write!(f, "{}", n),
            TrackerValue::Float(x) => write!(f, "{}", x),
        }
    }
}

/// A full todos row plus the aggregate completion count for the current
/// view/interval context (the `completions` column comes from the query).
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: Id,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: i64,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`] (see
    /// [`crate::date::span_to_db`]) — NOT seconds despite the name.
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: i32,
    pub end_time: Option<i64>,
    /// Parent task id (task tree); `None` for root-level tasks.
    pub parent: Option<Id>,
    pub completions: Option<i32>,
    pub last_time: Option<LogTime>,
    /// Location of the completion `last_time` names; empty when there is none.
    pub last_location: String,
}

impl<'r> FromRow<'r, SqliteRow> for TaskRow {
    fn from_row(row: &'r SqliteRow) -> Result<Self, sqlx::Error> {
        let last_time = match row.try_get::<Option<i64>, _>("last_time") {
            Ok(Some(epoch)) => Some(LogTime::new(epoch, log_offset(row, "last_tz_offset"))),
            Ok(None) => None,
            Err(sqlx::Error::ColumnNotFound(_)) => None,
            Err(err) => return Err(err),
        };
        let last_location = row
            .try_get::<Option<String>, _>("last_location")
            .ok()
            .flatten()
            .unwrap_or_default();
        Ok(Self {
            id: row.try_get("id")?,
            short_id: row.try_get("short_id")?,
            name: row.try_get("name")?,
            body: row.try_get("body")?,
            priority: row.try_get("priority")?,
            start_time: row.try_get("start_time")?,
            available_duration_secs: row.try_get("available_duration_secs")?,
            interval_secs: row.try_get("interval_secs")?,
            target_count: row.try_get("target_count")?,
            optional: row.try_get("optional")?,
            end_time: row.try_get("end_time")?,
            parent: row.try_get("parent")?,
            completions: row.try_get("completions")?,
            last_time,
            last_location,
        })
    }
}

impl TaskRow {
    pub fn is_recurring(&self) -> bool {
        self.interval_secs.is_some()
    }

    /// The recurrence interval as a calendar `jiff::Span`.
    pub fn interval_span(&self) -> Option<jiff::Span> {
        self.interval_secs.map(crate::date::db_to_span)
    }

    /// A scheduled task: no recurrence interval, with an availability
    /// window (`available_duration_secs`). Recurring tasks can carry an
    /// available duration too, so the interval check is what separates them.
    pub fn is_scheduled(&self) -> bool {
        self.interval_secs.is_none() && self.available_duration_secs.is_some()
    }

    /// The task's [`TaskKind`](crate::types::TaskKind), derived from its
    /// scheduling fields: recurring (has an interval) > scheduled
    /// (availability window, no interval) > oneshot. A target count changes
    /// completion behavior but does not change the task kind.
    pub fn kind(&self) -> TaskKind {
        TaskKind::of(self.interval_secs, self.available_duration_secs)
    }

    /// A completion sum as the readers use it: a scheduled task stores its
    /// state as a signed count (`1` done, `0` failed, `-1` unmarked), and
    /// unmarked means no completions at all.
    pub(crate) fn normalize_completions(&self, sum: Option<i32>) -> Option<i32> {
        match sum {
            Some(count) if self.is_scheduled() && count < 0 => None,
            other => other,
        }
    }

    pub fn is_done(&self) -> bool {
        if self.is_scheduled() {
            // Scheduled tasks are done when they have a completed entry
            // (>= 1) or their window has fully elapsed with no entry
            // (auto-completed). A failed entry (0) is not done.
            match self.completions {
                Some(c) if c > 0 => true,
                Some(_) => false,
                None => match self.available_duration_secs {
                    Some(dur) => self.start_time + dur < crate::date::now(),
                    None => false,
                },
            }
        } else {
            crate::task::is_task_done(self.target_count, self.completions)
        }
    }

    pub fn start_datetime(&self) -> String {
        crate::date::format_datetime(self.start_time)
    }

    pub fn end_datetime(&self, named_months: bool) -> Option<String> {
        self.end_time
            .map(|ts| crate::date::format_human_datetime(ts, named_months))
    }
}

/// A mood row for the tracker/today views.
#[derive(Debug, Clone)]
pub struct MoodRow {
    pub id: Id,
    pub mood: String,
    pub body: String,
    pub time: LogTime,
    pub location: String,
    pub embedding: Option<Vec<u8>>,
    /// Cached emotional-saliency score for the mood text, backfilled by
    /// `ColorAxes::mood_color_cached`; `None` until first computed.
    pub score: Option<f32>,
    pub duration: Option<i64>,
    pub todo_id: Option<Id>,
}

/// A tracker row with the score decoded as text (the `score` column is a
/// BLOB with dynamic typing; `CAST(score AS TEXT)` makes every storage type
/// decodable). `mood` is the mood row this tracker entry is attached to
/// (the `tracker.mood` column), when any — the today-view preview's `mood:`
/// field.
#[derive(Debug, Clone)]
pub struct TrackerEntryRow {
    pub id: Id,
    pub tracker_type: String,
    pub score: String,
    pub time: LogTime,
    pub location: String,
    pub mood: Option<Id>,
}

/// One `GROUP BY type, typeof(score)` bucket over the `tracker` table, for
/// `:tool db doctor`: the storage-class distribution of a tracker type's entries
/// plus how many of the integer entries are nonzero (informational; null
/// replacement markers store their slot position there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerScoreKindRow {
    pub tracker_type: String,
    /// SQLite storage class of the bucket: `integer`, `real`, or `text`
    /// (the column CHECK constrains `typeof(score)` to these three).
    pub storage: String,
    /// Entries in this bucket.
    pub count: i64,
    /// Of `count`, how many have `score != 0` (informational for integer
    /// buckets).
    pub nonzero: i64,
}

/// Recurring-task metadata used by the completion-dots tracker.
#[derive(Debug, Clone)]
pub struct RecurringTaskMeta {
    pub id: Id,
    /// Recurrence anchor; interval slots are computed from it
    /// (`start_time + span * k`).
    pub start_time: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`].
    pub interval_secs: Option<i64>,
    pub target_count: i32,
}

/// A completion event (time, location, count) for a task. `count` mirrors the i32
/// type of the `todo_completions.count` column (every writer binds an i32
/// value — see `update_task` and `set_scheduled_completion`).
#[derive(Debug, Clone)]
pub struct CompletionRow {
    pub time: LogTime,
    /// Location string committed when the completion was written; may be empty.
    pub location: String,
    pub count: i32,
}

/// A log timestamp read from the `time`/`tz_offset` columns.
pub fn log_time(row: &SqliteRow) -> LogTime {
    LogTime::new(row.get("time"), log_offset(row, "tz_offset"))
}

/// A stored `tz_offset` cell as seconds east of UTC.
///
/// Sync applies the cell as a 64-bit integer, so a foreign writer can store a
/// value `i32` cannot express. Such a value is not a UTC offset, so it reads as
/// UTC instead of failing the row that carries it.
pub fn log_offset(row: &SqliteRow, column: &str) -> i32 {
    row.try_get::<i64, _>(column)
        .ok()
        .and_then(|secs| i32::try_from(secs).ok())
        .unwrap_or(0)
}

/// A mood row read from a query selecting `id`, `mood`, `body`, `time`,
/// `tz_offset`, `location`, `embedding`, `score`, `duration`, and `todo_id`.
pub fn mood_row(row: &SqliteRow) -> MoodRow {
    MoodRow {
        id: row.get("id"),
        mood: row.get("mood"),
        body: row.get("body"),
        time: log_time(row),
        location: row.get("location"),
        embedding: row.get("embedding"),
        score: row.get("score"),
        duration: row.get("duration"),
        todo_id: row.get("todo_id"),
    }
}

/// A tracker entry read from a query selecting `id`, `type`, `score` (cast to
/// text), `time`, `tz_offset`, `location`, and `mood`.
pub fn tracker_entry_row(row: &SqliteRow) -> TrackerEntryRow {
    TrackerEntryRow {
        id: row.get("id"),
        tracker_type: row.get("type"),
        score: row.get("score"),
        time: log_time(row),
        location: row.get("location"),
        mood: row.get("mood"),
    }
}

/// A task deleted by `prune_tasks`, with the reason it was pruned. The
/// `short_id` is `None` for completed oneshot tasks (their id is cleared on
/// completion).
#[derive(Debug, Clone)]
pub struct PrunedTask {
    pub id: Id,
    pub short_id: Option<i64>,
    pub name: String,
    pub reason: String,
}

/// Task identity + completion state for the `- <short-id> [count]` update
/// command. `id` is the stable row id; `short_id` is the user-facing id
/// (`None` once the task is completed).
#[derive(Debug, Clone)]
pub struct TaskUpdateInfo {
    pub id: Id,
    pub short_id: Option<i64>,
    pub name: String,
    pub target_count: i32,
    pub prior_completions: i32,
}

/// One availability window of a recurring task, with the completion
/// aggregates scoped to that window's interval.
#[derive(Debug, Clone)]
pub struct RecurringWindow {
    /// The task row; `completions` and `last_time` are scoped to this
    /// window's interval (`[window_start, window_start + interval)`).
    pub task: TaskRow,
    /// Window start (the interval start).
    pub window_start: i64,
    /// Window end: the availability-window end — `window_start +
    /// available_duration_secs` (the whole interval when no duration is
    /// set or it covers the interval), capped at the task's `end_time`.
    pub window_end: i64,
}
