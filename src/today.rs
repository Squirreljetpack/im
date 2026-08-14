use anyhow::Result;
use ratatui::{backend::FromCrossterm, style::Color as RatColor};
use sqlx::SqlitePool;
use std::io::Write;

use crate::cli::CliOpts;
use crate::config::{Config, TrackerKind};
use crate::date::{self, Epoch};
use crate::db::TaskRow;
use crate::task::pending_sort_time;
use crate::types::{TaskKind, TasksFilter, TodayHorizon, ViewVariant};

/// Default glyph for tracker entries in the today view (overridable via
/// `[badges] tracker`). A named constant so the default can be adjusted
/// without touching the config docs.
pub(crate) const TEXT_ENTRY_BADGE: char = '◆';

/// Category of a today-view entry, driving routing (edit / delete / preview)
/// and presentation. Replaces the old `entry_type` string and the task-only
/// `interval_secs` marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// A task, carrying its [`TaskKind`].
    Task(TaskKind),
    /// Mood entry carrying a mood label.
    Mood,
    /// Journal-only mood entry (empty mood label; the body holds the text).
    Journal,
    /// Tracker entry, carrying the tracker's configured payload kind.
    Tracker(TrackerKind),
}

impl EntryKind {
    pub fn is_task(self) -> bool {
        matches!(self, Self::Task(_))
    }

    pub fn is_mood(self) -> bool {
        matches!(self, Self::Mood | Self::Journal)
    }

    pub fn is_tracker(self) -> bool {
        matches!(self, Self::Tracker(_))
    }
}

/// A tracker entry attached to a mood row (the `tracker.mood`
/// column), pre-rendered for the preview's `linked:` section: the tracker
/// name (in the same color the tracker's badge would use for this value)
/// plus the value payload.
#[derive(Debug, Clone)]
pub struct LinkedTracker {
    pub name: String,
    pub payload: String,
    pub color: RatColor,
}

/// A task linked to a mood via `task_moods`, pre-rendered for the
/// preview's `linked:` section: the badge glyph with its color plus the
/// task name.
#[derive(Debug, Clone)]
pub struct LinkedTask {
    pub badge: Option<char>,
    pub color: RatColor,
    pub name: String,
}

/// Data for a single today-view entry.
#[derive(Debug, Clone)]

pub struct TodayEntry {
    pub id: Option<i64>,
    pub time: i64,
    /// Rendered time-cell text: "HH:MM", "Tu HH:MM" (two-letter weekday
    /// prefix for entries outside the anchored day), or empty for entries
    /// with no displayable time (all-day recurring tasks, undated oneshots)
    /// — those sort after all timed entries.
    pub time_label: String,
    pub kind: EntryKind,
    pub label: String,
    pub body: String,
    pub task_id: Option<i64>,
    pub priority: i32,
    /// Task entries only: the task row the badge rules are derived from
    /// at render time (window-scoped for recurring windows). `None` for
    /// every other entry kind.
    pub task: Option<TaskRow>,
    /// Tracker entries only: the decoded score (`None` for text
    /// trackers, which have no score). Drives the render-time badge
    /// color binning.
    pub score: Option<f64>,
    /// Tracker entries only: the mood text this tracker entry is
    /// attached to (`tracker.mood`), for the preview's `mood:` field.
    /// `None` for every other entry kind. The mood's color resolves via
    /// the process-wide mood-color cache; the raw row (embedding and
    /// cached saliency score) rides the fetch's `mood_rows` handoff.
    pub linked_mood: Option<String>,
    /// Recurring-task entries only: the availability window this row
    /// represents, with the window-scoped task row (completions and
    /// `last_time` limited to the window's interval). `None` for every
    /// other entry kind. Drives the D10 confirm (`now >= window_end` on a
    /// not-done window) and the selection preview.
    pub recurring_window: Option<crate::db::RecurringWindow>,
    /// Tracker entries with a configured interval: the (anchor, span) pair,
    /// so the preview can show the next interval start like recurring tasks.
    pub tracker_interval: Option<(Epoch, jiff::Span)>,
    /// Tracker entries: the time of the previous entry of this kind
    /// (strictly earlier; the preview's `prev:` field). `None` when no
    /// earlier entry exists.
    pub tracker_prev: Option<Epoch>,
    /// Mood entries only: trackers attached to the mood row
    /// (`tracker.mood`) and tasks linked via `task_moods`, rendered as
    /// the preview's `linked:` section. Empty for every other entry kind.
    pub linked_trackers: Vec<LinkedTracker>,
    pub linked_tasks: Vec<LinkedTask>,
}

impl TodayEntry {
    /// Derive the row's badge — glyph and color — at render time. The badge
    /// is not stored on the entry; the raw inputs it needs (`task`, `score`,
    /// plus the config) are.
    ///
    /// Cache-read-only: the mood color is a pure lookup in the process-wide
    /// cache — a miss renders the neutral fallback without running the
    /// color pipeline (the pipeline is expensive; [`crate::color::compute_mood_colors`]
    /// fills the cache, in a background task in the TUI, synchronously in
    /// the CLI).
    pub fn badge(&self, config: &Config) -> (Option<char>, RatColor) {
        match self.kind {
            EntryKind::Mood => {
                let color = crate::color::cached_mood_color(&self.label)
                    .map(|oklab| {
                        let rgb = oklab.to_srgb();
                        RatColor::Rgb(rgb.r, rgb.g, rgb.b)
                    })
                    .unwrap_or(RatColor::DarkGray);
                (Some(config.badges.mood.unwrap_or('●')), color)
            }
            EntryKind::Journal => match &config.badges.journal_badge {
                Some(s) => (
                    s.badge,
                    s.color
                        .map(RatColor::from_crossterm)
                        .unwrap_or(RatColor::Reset),
                ),
                None => (None, RatColor::Reset),
            },
            EntryKind::Tracker(_) => {
                let glyph = config.badges.tracker.unwrap_or(TEXT_ENTRY_BADGE);
                let Some((tracker_type, _)) = self.label.split_once(':') else {
                    return (Some(glyph), RatColor::DarkGray);
                };
                let Some(tracker) = config.tracker.get(tracker_type.trim()) else {
                    return (Some(glyph), RatColor::DarkGray);
                };
                (
                    Some(glyph),
                    tracker_entry_color(tracker, &config.tasks.colors, self.time, self.score),
                )
            }
            EntryKind::Task(_) => {
                let Some(task) = &self.task else {
                    return (Some('○'), RatColor::Reset);
                };
                let (glyph, color) = if let Some(window) = &self.recurring_window {
                    crate::badge::recurring_window_badge(
                        task,
                        window.window_end,
                        config,
                        crate::date::now(),
                    )
                } else {
                    crate::badge::task_badge(task, config, false)
                };
                (Some(glyph), RatColor::from_crossterm(color))
            }
        }
    }
}

/// Today-view time cell for a timestamp: "HH:MM" when it falls on the
/// anchored day, "Tu HH:MM" (two-letter weekday prefix) when it falls
/// within a week of it — entries outside the anchored day stay
/// distinguishable in the +tomorrow/+week horizons — and the compact
/// day-time form ("DD HH:MM") outside that week entirely.
fn today_time_label(time: i64, day_start_epoch: i64) -> String {
    if time < day_start_epoch || time > day_start_epoch + 7 * 86_400 {
        crate::date::format_day_time(time)
    } else if crate::date::day_start(time) == day_start_epoch {
        crate::date::format_time(time)
    } else {
        format!(
            "{} {}",
            crate::date::format_weekday(time),
            crate::date::format_time(time)
        )
    }
}

/// The today-view time cell for a task row: "HH:MM" (weekday prefix when
/// outside the anchored day) for timed rows — completion time when done,
/// otherwise the task's deadline/availability end — and empty for the
/// untimed group (undated oneshots). Shared with the tasks view's date
/// column.
pub(crate) fn task_time_label(task: &TaskRow, time: i64, day_start_epoch: i64) -> String {
    if task.is_done() {
        return today_time_label(time, day_start_epoch);
    }
    if !task.is_scheduled() && !task.is_recurring() && task.end_time.is_none() {
        // Undated oneshot.
        return String::new();
    }
    today_time_label(time, day_start_epoch)
}

/// Today-view time for a recurring availability window (one row per
/// window): a completed window — or one that has passed (`now >=
/// window_end`) — shows the last completion within its interval, else the
/// window end; an open or future window shows the window start.
fn recurring_window_time(w: &crate::db::RecurringWindow, now: i64) -> i64 {
    if w.task.is_done() || now >= w.window_end {
        w.task.last_time.unwrap_or(w.window_end)
    } else {
        w.window_start
    }
}

/// Today-view sort: timed entries first (by timestamp ascending); then the
/// no-time group (undated oneshots) by priority descending, then by
/// untruncated availability end ascending.
pub(crate) fn today_sort(a: &TodayEntry, b: &TodayEntry) -> std::cmp::Ordering {
    let (a_blank, b_blank) = (a.time_label.is_empty(), b.time_label.is_empty());
    match (a_blank, b_blank) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        (false, false) => a.time.cmp(&b.time),
        (true, true) => b.priority.cmp(&a.priority).then(a.time.cmp(&b.time)),
    }
}

/// The color of a tracker entry's badge for the given decoded score: Null
/// trackers with an interval use the time-of-day coloring; numeric trackers
/// bin the score over the configured palette; text trackers use the
/// single-color palette override (validated to exactly 1 entry in
/// Config::init) or neutral gray. Shared by the main tracker rows and the
/// linked-tracker lines in mood previews.
fn tracker_entry_color(
    tracker: &crate::config::TrackerSetting,
    task_colors: &crate::config::ColorBins,
    time: i64,
    score: Option<f64>,
) -> RatColor {
    let colors = tracker.colors.as_ref().unwrap_or(task_colors);
    match tracker.kind {
        TrackerKind::Null => RatColor::from_crossterm(crate::badge::null_tracker_color(
            colors,
            tracker,
            time,
            score.unwrap_or(0.0),
        )),
        // Text entries have no score; a single-color palette
        // override (validated to exactly 1 entry in Config::init)
        // colors their badge, otherwise neutral gray.
        TrackerKind::Text => tracker
            .colors
            .as_ref()
            .and_then(|c| c.first())
            .map(|c| RatColor::from_crossterm(*c))
            .unwrap_or(RatColor::DarkGray),
        _ => match score {
            Some(s) => RatColor::from_crossterm(crate::badge::tracker_color(
                colors,
                s,
                tracker.min,
                tracker.max,
            )),
            None => RatColor::DarkGray,
        },
    }
}

/// Fill `cache` with the color of every mood entry (the full color
/// pipeline: stored-embedding decode or on-the-fly embedding, NNLS
/// regression, saliency prediction). Entries already in the cache are
/// skipped via the cache itself.
///
/// The today fetch's output: the rendered entries plus the raw mood rows
/// (mood entries and tracker-linked moods, deduped by row id). The rows'
/// embeddings ride this handoff into [`crate::color::compute_mood_colors`]
/// by move — never copied onto the entries themselves.
#[derive(Default)]
pub struct TodayFetch {
    pub entries: Vec<TodayEntry>,
    pub mood_rows: Vec<crate::db::MoodRow>,
}

/// Fetch all today-view entries within the given horizon.
///
/// All variants share the same task base — tasks active at any point
/// during the period (interval-aware availability-window overlap for
/// recurring). `show` selects what rides on top: `All` also merges tasks
/// with a completion today (time = last completion); `A` filters completed
/// tasks out and shows no completions; `B` is the same as `All` but
/// tasks-only (no moods/trackers) and carries `coalesce_completions`
/// (D11 — no behavior yet). The oneshot section's task filter is bound to
/// the variant: `All` uses `config.today_view.initial_tasks_filter`, `A`
/// pins `Horizon`, `B` pins `Overdue`. See docs/VIEWS.md.
pub async fn fetch_today_entries(
    pool: &SqlitePool,
    config: &Config,
    horizon: TodayHorizon,
    day_epoch: Option<i64>,
    show: ViewVariant,
) -> Result<TodayFetch> {
    // The oneshot filter is bound to the view variant: `All` keeps the
    // configured filter (default `All` — any oneshot task), `A` pins the
    // old default (open tasks due within the horizon), `B` pins the old
    // include_overdue behavior (dated tasks due in the horizon or
    // overdue). Undated open tasks therefore surface in `All` (with the
    // default filter) — the today view's incomplete-tasks guarantee.
    let filter = match show {
        ViewVariant::All => config.today_view.initial_tasks_filter,
        ViewVariant::A => TasksFilter::Horizon,
        ViewVariant::B => TasksFilter::Overdue,
    };
    // `None` (journal-only mode) hides the entire task section — the
    // scheduled/recurring fetches below return empty lists.
    let tasks_enabled = filter != TasksFilter::None;
    // `im @<date>` anchors the day; bare `im` is today.
    let day_start_epoch = day_epoch.unwrap_or_else(date::today_start);
    let day_end_epoch = date::day_end(day_start_epoch);
    let horizon_end = horizon.end_epoch(day_start_epoch);
    let now_ts = date::now();

    let mut entries: Vec<TodayEntry> = Vec::new();
    // Raw mood rows for the color handoff (mood entries plus tracker-
    // linked moods), deduped by row id at the end.
    let mut mood_rows: Vec<crate::db::MoodRow> = Vec::new();

    // B is tasks-only: no moods, no tracker entries.
    if show != ViewVariant::B {
        // 1. Moods within the horizon (day start through horizon end,
        // matching the task fetches below).
        let moods = crate::db::fetch_moods_between(pool, day_start_epoch, horizon_end).await?;

        // Tracker entries and tasks attached to these moods (the mood
        // preview's `linked:` section).
        let mood_ids: Vec<i64> = moods.iter().map(|f| f.id).collect();
        let linked_trackers = crate::db::fetch_mood_trackers(pool, &mood_ids).await?;
        let linked_tasks = crate::db::fetch_mood_tasks(pool, &mood_ids).await?;

        for mut f in moods {
            let id = f.id;
            let mood = f.mood.clone();
            // Take the body for the entry; the handoff row keeps its
            // embedding/score and needs no body.
            let body = std::mem::take(&mut f.body);
            let time = f.time;
            // The row itself rides the color handoff (see `TodayFetch`).
            mood_rows.push(f);
            // Trackers attached to this mood: name in the tracker's own
            // color, payload per kind (text/number/float values; null
            // trackers carry none). Trackers no longer in the config can't
            // be resolved — skipped (unlike the main tracker rows, which
            // error).
            let mut l_trackers = Vec::new();
            if let Some(rows) = linked_trackers.get(&id) {
                for row in rows {
                    let Some(tracker) = config.tracker.get(&row.tracker_type) else {
                        continue;
                    };
                    let (payload, score) = match tracker.kind {
                        TrackerKind::Text => (row.score.clone(), None),
                        TrackerKind::Number | TrackerKind::Float => {
                            let score = crate::tracker::score_f64(&row.score);
                            (score.to_string(), Some(score))
                        }
                        TrackerKind::Null => {
                            let score = crate::tracker::score_f64(&row.score);
                            // Count mode (either bound missing) shows the
                            // count; with both bounds the entry is a time
                            // marker and shows the moment.
                            let payload = if tracker.min.is_none() || tracker.max.is_none() {
                                score.to_string()
                            } else {
                                date::format_datetime_short(row.time)
                            };
                            (payload, Some(score))
                        }
                    };
                    l_trackers.push(LinkedTracker {
                        name: row.tracker_type.clone(),
                        payload,
                        color: tracker_entry_color(tracker, &config.tasks.colors, row.time, score),
                    });
                }
            }
            // Tasks linked via `task_moods`: badge + color like the today
            // view's own task rows.
            let mut l_tasks = Vec::new();
            if let Some(tasks) = linked_tasks.get(&id) {
                for task in tasks {
                    let (badge, color) = crate::badge::task_badge(task, config, false);
                    l_tasks.push(LinkedTask {
                        badge: Some(badge),
                        color: RatColor::from_crossterm(color),
                        name: task.name.clone(),
                    });
                }
            }
            entries.push(TodayEntry {
                id: Some(id),
                time,
                time_label: today_time_label(time, day_start_epoch),
                kind: if mood.is_empty() {
                    EntryKind::Journal
                } else {
                    EntryKind::Mood
                },
                label: mood,
                body,
                task_id: None,
                priority: 0,
                task: None,
                score: None,
                linked_mood: None,
                recurring_window: None,
                tracker_interval: None,
                tracker_prev: None,
                linked_trackers: l_trackers,
                linked_tasks: l_tasks,
            });
        }

        // 2. Tracker entries within the horizon.
        let trackers =
            crate::db::fetch_tracker_entries_today(pool, day_start_epoch, horizon_end).await?;
        // Previous entry time per tracker entry (the preview `prev:` field).
        let tracker_prevs =
            crate::db::fetch_tracker_prev_times(pool, day_start_epoch, horizon_end).await?;

        // The mood each tracker entry is attached to (`tracker.mood`),
        // batch-fetched for the preview's `mood:` field.
        let linked_moods_by_id = crate::db::fetch_moods_by_ids(
            pool,
            &trackers.iter().filter_map(|t| t.mood).collect::<Vec<_>>(),
        )
        .await?;

        for row in trackers {
            let tracker_id = row.id;
            let tracker_type = row.tracker_type;
            let time = row.time;
            let tracker = config.tracker.get(&tracker_type).ok_or_else(|| {
                anyhow::anyhow!("Unknown tracker '{}' not found in config", tracker_type)
            })?;
            let (label, score) = match tracker.kind {
                TrackerKind::Text => (format!("{}: {}", tracker_type, row.score), None),
                TrackerKind::Number | TrackerKind::Float => {
                    let score = crate::tracker::score_f64(&row.score);
                    (format!("{}: {}", tracker_type, score), Some(score))
                }
                // Null payloads carry no value: count mode (either bound
                // missing) shows the count, with both bounds the entry is a
                // time marker and shows the moment (`sleep: 3-15 14:30`).
                TrackerKind::Null => {
                    let score = crate::tracker::score_f64(&row.score);
                    let payload = if tracker.min.is_none() || tracker.max.is_none() {
                        score.to_string()
                    } else {
                        date::format_datetime_short(time)
                    };
                    (format!("{}: {}", tracker_type, payload), Some(score))
                }
            };
            entries.push(TodayEntry {
                id: Some(tracker_id),
                time,
                time_label: today_time_label(time, day_start_epoch),
                kind: EntryKind::Tracker(tracker.kind),
                label,
                body: String::new(),
                task_id: None,
                priority: 0,
                task: None,
                score,
                linked_mood: row
                    .mood
                    .and_then(|mid| linked_moods_by_id.get(&mid).map(|m| m.mood.clone())),
                recurring_window: None,
                tracker_interval: tracker.interval.map(|iv| (iv.anchor, iv.span)),
                tracker_prev: tracker_prevs.get(&tracker_id).copied().flatten(),
                linked_trackers: Vec::new(),
                linked_tasks: Vec::new(),
            });
        }
    } // show != ShowVariant::B

    // 3. Oneshot tasks per the variant-bound task filter (see the match
    // above): `Horizon` keeps tasks due from today through the horizon
    // end; `Overdue` adds dated tasks due before today (undated tasks are
    // never overdue, so they stay out); `Pending` and `All` have no date
    // bounds. Every filter is incomplete-only — the completed-today merge
    // in step 5 surfaces tasks completed today.
    let due_tasks = if tasks_enabled {
        crate::db::fetch_oneshot_tasks(pool, filter, horizon_end, day_start_epoch).await?
    } else {
        Vec::new()
    };

    for task in &due_tasks {
        // A filters completed tasks out.
        if show == ViewVariant::A && task.is_done() {
            continue;
        }
        // Time: done → completion time; else the due time (`end_time` when
        // set — `! name @<time>`; undated oneshots are untimed).
        let time = pending_sort_time(task, now_ts);
        let time_label = task_time_label(task, time, day_start_epoch);
        entries.push(TodayEntry {
            id: None,
            time,
            time_label,
            kind: EntryKind::Task(task.kind()),
            label: task.name.clone(),
            body: task.body.clone(),
            task_id: Some(task.id),
            priority: task.priority,
            task: Some(task.clone()),
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
        });
    }

    // 3b. Scheduled tasks overlapping the horizon (window overlap: started
    // before horizon_end, still open past today_start). All states show —
    // ongoing, completed / auto-completed, failed — with the same badge
    // semantics as the tasks app.
    let scheduled_tasks = if tasks_enabled {
        crate::db::fetch_scheduled_today(pool, horizon_end, day_start_epoch).await?
    } else {
        Vec::new()
    };

    for task in &scheduled_tasks {
        // A filters completed tasks out (incl. auto-completed).
        if show == ViewVariant::A && task.is_done() {
            continue;
        }
        // Time: done → completion time (auto-completed has no entry, so it
        // falls back to the window end); else `start_time`.
        let time = pending_sort_time(task, now_ts);
        let time_label = task_time_label(task, time, day_start_epoch);
        entries.push(TodayEntry {
            id: None,
            time,
            time_label,
            kind: EntryKind::Task(task.kind()),
            label: task.name.clone(),
            body: task.body.clone(),
            task_id: Some(task.id),
            priority: task.priority,
            task: Some(task.clone()),
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
        });
    }

    // 4. Recurring tasks: one entry per availability window intersecting
    // the period (all variants; interval-aware availability-window overlap
    // — VIEWS.md). Each window's completions / last completion are scoped
    // to its own interval, so time, done state, and badge are per window.
    // `B` keeps only the next (earliest) window per task.
    let recurring_windows = if tasks_enabled {
        crate::db::fetch_recurring_windows_for_period(pool, day_start_epoch, horizon_end).await?
    } else {
        Vec::new()
    };

    let mut seen_recurring = std::collections::HashSet::new();
    for w in &recurring_windows {
        // B: only the next recurring window per task.
        if show == ViewVariant::B && !seen_recurring.insert(w.task.id) {
            continue;
        }
        // A filters completed windows out (the window's own completion
        // state, not the current interval's).
        if show == ViewVariant::A && w.task.is_done() {
            continue;
        }
        // Time (VIEWS.md): a completed or passed (`now >= window_end`)
        // window shows the last completion within its interval, else the
        // window end; an open or future window shows the window start.
        let time = recurring_window_time(w, now_ts);
        let time_label = task_time_label(&w.task, time, day_start_epoch);
        entries.push(TodayEntry {
            id: None,
            time,
            time_label,
            kind: EntryKind::Task(w.task.kind()),
            label: w.task.name.clone(),
            body: w.task.body.clone(),
            task_id: Some(w.task.id),
            priority: w.task.priority,
            task: Some(w.task.clone()),
            score: None,
            linked_mood: None,
            recurring_window: Some(w.clone()),
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
        });
    }

    // 5. Tasks with a completion entry today (All and B — B is the same as
    // All minus the moods/trackers sections): merged over the regular
    // rows (dedup by task_id — the completed-today row wins, time = last
    // completion timestamp) so a task completed today shows its completion
    // time even when it is no longer active (or not in the regular lists
    // at all). Recurring tasks with a per-window entry (step 4) are
    // skipped: the window rows already carry the window-scoped completion
    // state and rule-based times. `A` filters completed tasks out, so the
    // fetch is skipped there.
    if tasks_enabled && show != ViewVariant::A {
        let completed_today =
            crate::db::fetch_tasks_completed_on(pool, day_start_epoch, day_end_epoch).await?;
        for task in &completed_today {
            // Recurring windows already have entries (step 4) carrying the
            // window-scoped completion state — merging here would override
            // the rule-based window time with a day-scoped one. Tasks with
            // no window row (expired chain with a late completion) still
            // merge in below.
            if task.is_recurring() && entries.iter().any(|e| e.task_id == Some(task.id)) {
                continue;
            }
            let last_time = task.last_time.unwrap_or(now_ts);
            let entry = TodayEntry {
                id: None,
                time: last_time,
                time_label: today_time_label(last_time, day_start_epoch),
                kind: EntryKind::Task(task.kind()),
                label: task.name.clone(),
                body: task.body.clone(),
                task_id: Some(task.id),
                priority: task.priority,
                task: Some(task.clone()),
                score: None,
                linked_mood: None,
                recurring_window: None,
                tracker_interval: None,
                tracker_prev: None,
                linked_trackers: Vec::new(),
                linked_tasks: Vec::new(),
            };
            match entries.iter_mut().find(|e| e.task_id == Some(task.id)) {
                Some(existing) => *existing = entry,
                None => entries.push(entry),
            }
        }
    }

    // Sort: timed entries first by timestamp, then the no-time group by
    // priority descending and untruncated availability end.
    entries.sort_by(today_sort);

    // Dedupe the handoff rows by id: a mood row appears twice when a
    // tracker entry is attached to it.
    let mut seen = std::collections::HashSet::new();
    mood_rows.retain(|r| seen.insert(r.id));
    Ok(TodayFetch { entries, mood_rows })
}

/// Handle today view (non-terminal output): displays today's moods, tracker
/// entries, and task activity as tab-separated rows. TUI dispatch is handled by
/// [`crate::commands::execute_command`].
pub async fn write_today_view<W: Write>(
    pool: &SqlitePool,
    config: &Config,
    axes: &crate::color::ColorAxes,
    day_epoch: Option<i64>,
    show: ViewVariant,
    horizon: TodayHorizon,
    _opts: &CliOpts,
    out: &mut W,
) -> Result<()> {
    let TodayFetch { entries, mood_rows } =
        fetch_today_entries(pool, config, horizon, day_epoch, show).await?;
    // The CLI prints colors, so it computes them synchronously before
    // formatting (unlike the TUI, which fills the cache in the background).
    crate::color::compute_mood_colors(&mood_rows, axes);

    if entries.is_empty() {
        writeln!(out, "Nothing logged today.")?;
        return Ok(());
    }

    write!(
        out,
        "{}",
        crate::output::format_today_simple(&entries, config)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_today_time_label() {
        // 2024-03-15 is a Friday; 2024-03-16 a Saturday.
        let day =
            crate::date::parse_datetime("2024-03-15 00:00", crate::date::DATE_DIALECT).unwrap();
        let same =
            crate::date::parse_datetime("2024-03-15 09:30", crate::date::DATE_DIALECT).unwrap();
        let next =
            crate::date::parse_datetime("2024-03-16 09:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(today_time_label(same, day), "09:30");
        assert_eq!(today_time_label(next, day), "Sa 09:30");
        // Outside the week window → compact day-time form ("DD HH:MM").
        let far =
            crate::date::parse_datetime("2024-03-25 09:30", crate::date::DATE_DIALECT).unwrap();
        let early =
            crate::date::parse_datetime("2024-03-01 09:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(today_time_label(far, day), "25 09:30");
        assert_eq!(today_time_label(early, day), "01 09:30");
        crate::date::parse_datetime("2024-03-25 09:30", crate::date::DATE_DIALECT).unwrap();
        // The weekday form covers days 1-6 after the anchor; the 7th day
        // (>= day_start + week) is already the short form.
        let within =
            crate::date::parse_datetime("2024-03-21 09:30", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(today_time_label(within, day), "Th 09:30");
        let boundary =
            crate::date::parse_datetime("2024-03-22 00:00", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(today_time_label(boundary, day), "Fr 00:00");
    }

    fn task_row(
        start_time: Option<i64>,
        available_duration_secs: Option<i64>,
        interval: Option<jiff::Span>,
        end_time: Option<i64>,
        completions: Option<i32>,
        last_time: Option<i64>,
    ) -> TaskRow {
        // The row stores the packed DbSpan.
        let interval_secs = interval.map(|s| crate::date::span_to_db(&s));
        TaskRow {
            id: 1,
            short_id: Some(1),
            name: "t".to_string(),
            body: String::new(),
            priority: 5,
            start_time,
            available_duration_secs,
            interval_secs,
            target_count: 0,
            optional: 0,
            end_time,
            parent: None,
            completions,
            last_time,
        }
    }

    #[test]
    fn test_pending_sort_time() {
        let day =
            crate::date::parse_datetime("2024-03-16 00:00", crate::date::DATE_DIALECT).unwrap();
        let anchor =
            crate::date::parse_datetime("2024-03-15 08:00", crate::date::DATE_DIALECT).unwrap();
        let now =
            crate::date::parse_datetime("2024-03-16 14:00", crate::date::DATE_DIALECT).unwrap();
        let at = |s: &str| crate::date::parse_datetime(s, crate::date::DATE_DIALECT).unwrap();
        let day_secs = 86400;
        let hour_secs = 3600;

        let check = |task: &TaskRow, expect_time: i64, expect_label: &str| {
            let time = crate::task::pending_sort_time(task, now);
            let label = task_time_label(task, time, day);
            assert_eq!(time, expect_time, "time for {}", task.name);
            assert_eq!(label, expect_label, "label for {}", task.name);
        };

        // Recurring with an availability window (08:00-09:00), window
        // already closed at 14:00: the next interval's start (17th 08:00),
        // with a weekday prefix (outside the anchored day).
        check(
            &task_row(
                Some(anchor),
                Some(hour_secs),
                Some(jiff::Span::new().days(1)),
                None,
                None,
                None,
            ),
            at("2024-03-17 08:00"),
            "Su 08:00",
        );
        // Same recurring window, still open (now 08:30, before the 09:00
        // end): the window end of the current interval.
        let open_now = at("2024-03-16 08:30");
        let open = task_row(
            Some(anchor),
            Some(hour_secs),
            Some(jiff::Span::new().days(1)),
            None,
            None,
            None,
        );
        assert_eq!(
            crate::task::pending_sort_time(&open, open_now),
            at("2024-03-16 09:00"),
            "window still open → window end"
        );
        // Recurring without an explicit duration: the whole interval is the
        // window, so the closed window defers to the next interval's start
        // (timed — every recurring window has a time cell now).
        check(
            &task_row(
                Some(anchor),
                None,
                Some(jiff::Span::new().days(1)),
                None,
                None,
                None,
            ),
            at("2024-03-17 08:00"),
            "Su 08:00",
        );
        // Recurring whose duration would swallow the whole interval
        // (dur == interval — not enforced at ingestion): deferred to the
        // next interval's start, like the untimed group.
        check(
            &task_row(
                Some(anchor),
                Some(day_secs),
                Some(jiff::Span::new().days(1)),
                None,
                None,
                None,
            ),
            at("2024-03-17 08:00"),
            "Su 08:00",
        );
        // Scheduled, not done (window still open): the deadline.
        check(
            &task_row(
                Some(at("2024-03-16 08:00")),
                Some(10 * hour_secs),
                None,
                None,
                None,
                None,
            ),
            at("2024-03-16 18:00"),
            "18:00",
        );
        // Scheduled, done with an entry: the completion time.
        check(
            &task_row(
                Some(at("2024-03-16 08:00")),
                Some(10 * hour_secs),
                None,
                None,
                Some(1),
                Some(at("2024-03-16 13:30")),
            ),
            at("2024-03-16 13:30"),
            "13:30",
        );
        // Scheduled, auto-completed (no entry, window elapsed): the window
        // end is the completion moment.
        check(
            &task_row(
                Some(at("2024-03-16 08:00")),
                Some(2 * hour_secs),
                None,
                None,
                None,
                None,
            ),
            at("2024-03-16 10:00"),
            "10:00",
        );
        // Oneshot, not done, with a due time.
        check(
            &task_row(
                Some(anchor),
                None,
                None,
                Some(at("2024-03-16 12:00")),
                None,
                None,
            ),
            at("2024-03-16 12:00"),
            "12:00",
        );
        // Oneshot, not done, undated: untimed (sorts last).
        check(
            &task_row(Some(anchor), None, None, None, None, None),
            i64::MAX,
            "",
        );
        // Oneshot, done: the completion time.
        check(
            &task_row(
                Some(anchor),
                None,
                None,
                Some(at("2024-03-16 12:00")),
                Some(1),
                Some(at("2024-03-16 13:00")),
            ),
            at("2024-03-16 13:00"),
            "13:00",
        );

        // `@done:b` partial history: recurring with target 2, one entry ever
        // — not done, so the pending key is the next interval's start (the
        // window closed at 09:00, before now); the done-view key is the
        // last completion entry.
        let partial = TaskRow {
            name: "partial history".to_string(),
            target_count: 2,
            ..task_row(
                Some(anchor),
                Some(hour_secs),
                Some(jiff::Span::new().days(1)),
                None,
                Some(1),
                Some(at("2024-03-16 13:00")),
            )
        };
        assert_eq!(
            crate::task::pending_sort_time(&partial, now),
            at("2024-03-17 08:00"),
            "pending view: next interval start (window passed)"
        );
        assert_eq!(
            crate::task::completed_sort_time(&partial),
            at("2024-03-16 13:00"),
            "done view: last completion entry"
        );
    }

    #[test]
    fn test_completed_sort_time() {
        let at = |s: &str| crate::date::parse_datetime(s, crate::date::DATE_DIALECT).unwrap();
        let day_secs = 86400;
        let hour_secs = 3600;

        // Done oneshot with an entry: the last completion entry.
        assert_eq!(
            crate::task::completed_sort_time(&task_row(
                Some(at("2024-03-16 08:00")),
                None,
                None,
                None,
                Some(1),
                Some(at("2024-03-16 13:00")),
            )),
            at("2024-03-16 13:00")
        );
        // Scheduled with an entry: the entry.
        assert_eq!(
            crate::task::completed_sort_time(&task_row(
                Some(at("2024-03-16 08:00")),
                Some(10 * hour_secs),
                None,
                None,
                Some(1),
                Some(at("2024-03-16 13:30")),
            )),
            at("2024-03-16 13:30")
        );
        // Scheduled without an entry (auto-completed): the window end.
        assert_eq!(
            crate::task::completed_sort_time(&task_row(
                Some(at("2024-03-16 08:00")),
                Some(2 * hour_secs),
                None,
                None,
                None,
                None,
            )),
            at("2024-03-16 10:00")
        );
        // Recurring, zero entries (`@done:b` history row): falls back to
        // the start time only — `available_duration_secs` is the
        // per-interval availability window, not a completion moment.
        assert_eq!(
            crate::task::completed_sort_time(&task_row(
                Some(at("2024-03-15 08:00")),
                Some(2 * hour_secs),
                Some(jiff::Span::new().days(1)),
                None,
                None,
                None,
            )),
            at("2024-03-15 08:00")
        );
        // Undated: i64::MAX (defensive — can't appear in a done view).
        assert_eq!(
            crate::task::completed_sort_time(&task_row(None, None, None, None, None, None)),
            i64::MAX
        );
    }

    #[test]
    fn test_today_sort() {
        let entry = |time: i64, time_label: &str, priority: i32| TodayEntry {
            id: None,
            time,
            time_label: time_label.to_string(),
            kind: EntryKind::Task(TaskKind::Oneshot),
            label: String::new(),
            body: String::new(),
            task_id: None,
            priority,
            task: None,
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
        };
        let mut entries = [
            entry(200, "20:00", 1),
            entry(350, "", 5),
            entry(300, "", 2),
            entry(400, "", 5),
            entry(100, "10:00", 9),
        ];
        entries.sort_by(today_sort);
        let got: Vec<(i64, i32, String)> = entries
            .iter()
            .map(|e| (e.time, e.priority, e.time_label.clone()))
            .collect();
        // Timed entries first by timestamp; the no-time group then by
        // priority descending, then by untruncated availability end.
        assert_eq!(
            got,
            vec![
                (100, 9, "10:00".to_string()),
                (200, 1, "20:00".to_string()),
                (350, 5, String::new()),
                (400, 5, String::new()),
                (300, 2, String::new()),
            ]
        );
    }
}
