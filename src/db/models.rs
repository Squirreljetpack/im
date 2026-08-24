use sqlx::FromRow;

use crate::types::TaskKind;

/// A task as seen by the creation/edit flows. `id` is the stable row id
/// (`Some` for existing tasks; `None` for new tasks — the row id is
/// autoassigned at insert time). `short_id` is the user-facing id: always
/// `None` on new tasks (the SQL layer allocates it), and `None` for
/// existing oneshot tasks once they are completed.
#[derive(Debug, Clone)]
pub struct TaskObject {
    pub id: Option<i64>,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: Option<i64>,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`] (see
    /// [`crate::date::span_to_db`]) — NOT seconds despite the name.
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: bool,
    pub end_time: Option<i64>,
    /// Parent task id (task tree); `None` for root-level tasks. Not
    /// settable through the CLI yet — creation always inserts root tasks.
    pub parent: Option<i64>,
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
    pub id: i64,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: Option<i64>,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`].
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: bool,
    pub end_time: Option<i64>,
    pub parent: Option<i64>,
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
    pub time: i64,
    pub embedding: Option<Vec<u8>>,
    /// Cached emotional-saliency score for the mood text, computed at entry
    /// creation (`None` for journal-only rows or failed embeddings).
    pub score: Option<f32>,
    pub trackers: Vec<TrackerObject>,
    pub duration: Option<i64>,
    pub todo_id: Option<i64>,
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
#[derive(Debug, Clone, FromRow)]
pub struct TaskRow {
    pub id: i64,
    pub short_id: Option<i64>,
    pub name: String,
    pub body: String,
    pub priority: i32,
    pub start_time: Option<i64>,
    pub available_duration_secs: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`] (see
    /// [`crate::date::span_to_db`]) — NOT seconds despite the name.
    pub interval_secs: Option<i64>,
    pub target_count: i32,
    pub optional: i32,
    pub end_time: Option<i64>,
    /// Parent task id (task tree); `None` for root-level tasks.
    pub parent: Option<i64>,
    pub completions: Option<i32>,
    #[sqlx(default)]
    pub last_time: Option<i64>,
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
        if self.is_recurring() {
            TaskKind::Recurring
        } else if self.is_scheduled() {
            TaskKind::Scheduled
        } else {
            TaskKind::Oneshot
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
                None => match (self.start_time, self.available_duration_secs) {
                    (Some(st), Some(dur)) => st + dur < crate::date::now(),
                    _ => false,
                },
            }
        } else {
            crate::task::is_task_done(self.target_count, self.completions)
        }
    }

    pub fn start_datetime(&self) -> Option<String> {
        self.start_time.map(crate::date::format_datetime)
    }

    pub fn end_datetime(&self, named_months: bool) -> Option<String> {
        self.end_time
            .map(|ts| crate::date::format_human_datetime(ts, named_months))
    }
}

/// A mood row for the tracker/today views.
#[derive(Debug, Clone)]
pub struct MoodRow {
    pub id: i64,
    pub mood: String,
    pub body: String,
    pub time: i64,
    pub embedding: Option<Vec<u8>>,
    /// Cached emotional-saliency score for the mood text, backfilled by
    /// `ColorAxes::mood_color_cached`; `None` until first computed.
    pub score: Option<f32>,
    pub duration: Option<i64>,
    pub todo_id: Option<i64>,
}

/// A tracker row with the score decoded as text (the `score` column is a
/// BLOB with dynamic typing; `CAST(score AS TEXT)` makes every storage type
/// decodable). `mood` is the mood row this tracker entry is attached to
/// (the `tracker.mood` column), when any — the today-view preview's `mood:`
/// field.
#[derive(Debug, Clone)]
pub struct TrackerEntryRow {
    pub id: i64,
    pub tracker_type: String,
    pub score: String,
    pub time: i64,
    pub mood: Option<i64>,
}

/// One `GROUP BY type, typeof(score)` bucket over the `tracker` table, for
/// `:db doctor`: the storage-class distribution of a tracker type's entries
/// plus how many of the integer entries are nonzero (time-marker null
/// trackers must carry score 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerScoreKindRow {
    pub tracker_type: String,
    /// SQLite storage class of the bucket: `integer`, `real`, or `text`
    /// (the column CHECK constrains `typeof(score)` to these three).
    pub storage: String,
    /// Entries in this bucket.
    pub count: i64,
    /// Of `count`, how many have `score != 0` (only meaningful for integer
    /// buckets).
    pub nonzero: i64,
}

/// Recurring-task metadata used by the completion-dots tracker.
#[derive(Debug, Clone)]
pub struct RecurringTaskMeta {
    pub id: i64,
    /// Recurrence anchor; interval slots are computed from it
    /// (`start_time + span * k`).
    pub start_time: Option<i64>,
    /// Recurrence interval as a packed [`crate::date::DbSpan`].
    pub interval_secs: Option<i64>,
    pub target_count: i32,
}

/// A completion event (time, count) for a task. `count` mirrors the i32
/// type of the `todo_completions.count` column (every writer binds an i32
/// value — see `update_task` and `set_scheduled_completion`).
#[derive(Debug, Clone)]
pub struct CompletionRow {
    pub time: i64,
    pub count: i32,
}

/// A task deleted by `prune_tasks`, with the reason it was pruned. The
/// `short_id` is `None` for completed oneshot tasks (their id is cleared on
/// completion).
#[derive(Debug, Clone)]
pub struct PrunedTask {
    pub id: i64,
    pub short_id: Option<i64>,
    pub name: String,
    pub reason: String,
}

/// Task identity + completion state for the `- <short-id> [count]` update
/// command. `id` is the stable row id; `short_id` is the user-facing id
/// (`None` once the task is completed).
#[derive(Debug, Clone)]
pub struct TaskUpdateInfo {
    pub id: i64,
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
    /// window's interval (`[window_start, window_start + interval)`);
    /// `end_time` carries the task's unscoped last completion instead of
    /// the expiry (the today view doesn't use the expiry).
    pub task: TaskRow,
    /// Window start (the interval start).
    pub window_start: i64,
    /// Window end: the availability-window end — `window_start +
    /// available_duration_secs` (the whole interval when no duration is
    /// set or it covers the interval), capped at the task's `end_time`.
    pub window_end: i64,
}
