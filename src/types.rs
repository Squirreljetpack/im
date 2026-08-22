use crate::date::{self, Epoch};

/// Category of a task, derived from its scheduling fields or selected during creation.
///
/// A target count distinguishes threshold-style completion behavior, but does not
/// create a separate task kind: those tasks are still one-shot tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// One-shot task, with or without a completion target.
    Oneshot,
    /// Recurring task (has an interval).
    Recurring,
    /// Scheduled task (has an availability window and no interval).
    Scheduled,
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TaskKind::Oneshot => "oneshot",
            TaskKind::Recurring => "recurring",
            TaskKind::Scheduled => "scheduled",
        })
    }
}

/// The task-list mode selected by `@` and `@done` views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    PendingTasks,
    DoneTasks,
}

/// Shared view subset control used by both the task and today TUIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewVariant {
    #[default]
    All,
    A,
    B,
}

impl ViewVariant {
    /// TUI title label for the today app: `[show: all|journal|due]`.
    /// `A` is "journal" (moods, trackers, and task completion events);
    /// `B` is "due" (overdue/due tasks, scheduled, recurring).
    pub fn today_label(&self) -> &'static str {
        match self {
            ViewVariant::All => "all",
            ViewVariant::A => "journal",
            ViewVariant::B => "due",
        }
    }

    /// TUI title label for the tasks app: `[show: all|oneshot|other]`
    /// (`A` = oneshots only, `B` = recurring + scheduled).
    pub fn tasks_label(&self) -> &'static str {
        match self {
            ViewVariant::All => "all",
            ViewVariant::A => "oneshot",
            ViewVariant::B => "other",
        }
    }

    /// Cycle order: All → A → B → All.
    pub fn next(&self) -> Self {
        match self {
            ViewVariant::All => ViewVariant::A,
            ViewVariant::A => ViewVariant::B,
            ViewVariant::B => ViewVariant::All,
        }
    }
}

/// Oneshot tasks filter used by internal task fetchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TasksFilter {
    /// No tasks at all — the entire task section (oneshots, scheduled,
    /// recurring, completed-today) is hidden, leaving a journal-only view.
    None,
    /// Any open oneshot task, any date, no bounds. Completed oneshots
    /// drop out of the regular lists — tasks completed today surface
    /// through the completed-today section instead.
    #[default]
    All,
    /// Only dated oneshots due within the horizon or overdue (`end_time`
    /// set and <= horizon end). Undated tasks are never overdue, so they
    /// don't appear here — use `Pending` or `All`.
    Due,
    /// Open (incomplete) oneshot tasks, any date — the undated-inbox view.
    Pending,
    /// Open oneshot tasks due within the horizon; overdue ones (due before
    /// the day start) excluded.
    Horizon,
}

/// How far ahead to include incomplete todos in the today view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodayHorizon {
    Today,
    Tomorrow,
    Week,
}

impl TodayHorizon {
    pub fn next(&self) -> Self {
        match self {
            TodayHorizon::Today => TodayHorizon::Tomorrow,
            TodayHorizon::Tomorrow => TodayHorizon::Week,
            TodayHorizon::Week => TodayHorizon::Today,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            TodayHorizon::Today => "today",
            TodayHorizon::Tomorrow => "+tomorrow",
            TodayHorizon::Week => "+this week",
        }
    }

    /// End of the horizon (inclusive) as epoch seconds, relative to the
    /// anchored day (its day-start). `Week` is always the next 7 days from
    /// the anchored day.
    pub fn end_epoch(&self, day_start: i64) -> i64 {
        match self {
            TodayHorizon::Today => date::day_end(day_start),
            TodayHorizon::Tomorrow => date::day_end(day_start + 86400),
            TodayHorizon::Week => date::day_end(day_start + 6 * 86400),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRef {
    Pick,
    Id(i64),
    Words(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub mood: String,
    // Raw tracker values ("-type value"): interpreted per the tracker's
    // declared kind (text/number/float/null) at write time in handle_entry.
    pub trackers: Vec<(String, String)>,
    /// Optional task reference (`+[id]` / `+[words]` / bare `+` to pick):
    /// resolved to one task row and linked to the mood entry at write time
    /// (a plain link via `mood.todo_id`, not a completion). At most one per
    /// entry.
    pub task_ref: Option<TaskRef>,
    /// Completion delta carried by a nonempty `+<ref>` followed by a plain
    /// numeric word (`+7 2`, `good +task 3`): applied to the resolved task
    /// like the old update command. A bare `+` never takes a payload;
    /// without a payload the ref is a link only.
    pub count: Option<i32>,
    /// Body text: `Ok(text)` when words followed the body delimiter; `Err(n)`
    /// when the delimiter was bare or absent — `n` is the delimiter's dot
    /// count (0 = no delimiter). `Err(n > 0)` opens the body editor in the
    /// handler with the `n`th template (1-based; see `open_editor_for_body`).
    pub body: Result<String, usize>,
    /// Session duration in seconds when logged as a timed mood session (`%<duration>`).
    pub duration: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub task_type: TaskKind,
    pub name: Option<String>,
    pub priority: Option<i32>,
    /// Start/due time for oneshot and scheduled creations (`! name @<time>`),
    /// resolved to an epoch at CLI parse time (`DATE_DIALECT`).
    pub date: Option<Epoch>,
    /// Body text: `Ok(text)` when words followed the body delimiter; `Err(n)`
    /// when the delimiter was bare or absent — `n` is the delimiter's dot
    /// count (0 = no delimiter). `Err(n > 0)` opens the body editor in the
    /// handler with the `n`th template (1-based; see `open_editor_for_body`).
    pub body: Result<String, usize>,
    /// Pre-filled name for interactive recurring creation
    /// (`im ! % <name>`), like oneshot creation where the
    /// name comes from the command line. `Some` always implies creation.
    pub prefill: Option<String>,
    /// Parent task reference from `! +<parent_id>` / `! +<words>` / `! +`;
    /// `None` for a root-level task. Resolved to a row id at creation time.
    pub parent: Option<TaskRef>,
    /// Available duration in seconds for scheduled creation
    /// (`! @<time>; …; %<duration>`) or recurring creation (`! %<duration>`);
    /// carried into the interactive flow so the duration prompt can be skipped
    /// when it came from the command line.
    pub available_duration: Option<Epoch>,
    /// `true` when a bare `%` was passed on recurring creation (`! %`) to prompt for the duration.
    pub pick_duration: bool,
}
