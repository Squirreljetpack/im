//! Seed the im database with all variations of mood entries,
//! tracker entries, oneshot tasks, recurring tasks, and
//! scheduled tasks — constructed directly as `sql::EntryObject` /
//! `sql::TaskObject` payloads (no CLI, no interactive prompts, no
//! command dispatch).
//!
//! Mood entries are seeded with `embedding: None`: the mood-grid and
//! today views recompute embeddings on the fly and backfill them into
//! the DB on first render, so no model work happens at seed time.
//!
//! Entries are deliberately spread across time frames — today, yesterday,
//! last week, a near-full rolling month window, and a near-full
//! calendar-year window — so the week / month / year tracker grids show
//! real structure instead of a single same-day blob.
//!
//! Usage: seed-db <DB_PATH>

use std::path::PathBuf;

use anyhow::{Context, Result};
use im::config::{Config, TrackerKind};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use im::date;
use im::db::{
    create_entry, create_task, set_scheduled_completion, update_task, EntryObject, TaskObject,
    TrackerObject, TrackerValue,
};

fn main() -> Result<()> {
    let db_path = std::env::args()
        .nth(1)
        .context("Usage: seed-db <DB_PATH>")?;

    // Load dev.toml explicitly so this works in release builds too.
    let toml_str = include_str!("../assets/dev.toml");
    let config: Config =
        toml::from_str(toml_str).context("assets/dev.toml must parse into Config")?;

    // Create the pool, run migrations, and populate.
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    rt.block_on(async {
        let pool = im::db::init_database(&PathBuf::from(&db_path))
            .await
            .context("Failed to open DB")?;

        populate(&pool, &config).await?;

        println!("Seeded database: {}", db_path);
        Ok::<_, anyhow::Error>(())
    })
}

async fn populate(pool: &sqlx::SqlitePool, config: &Config) -> Result<()> {
    let now = date::now();
    let today = date::today_start();
    let yesterday = date::day_start(today - 86400);
    let tomorrow = date::day_start(today + 86400);

    // 1. Mood & tracker entries (embedding: None everywhere).
    seed_mood_entries(pool, config, today, yesterday).await?;

    // 2. Oneshot tasks.
    seed_oneshot_tasks(pool, now, today, yesterday, tomorrow).await?;

    // 3. Recurring tasks.
    seed_recurring_tasks(pool, now).await?;

    // 4. Scheduled tasks.
    seed_scheduled_tasks(pool, now, today, yesterday).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Mood & tracker entries
// ---------------------------------------------------------------------------

/// Mood + tracker entries, each with `embedding: None`. Times are
/// chosen so every tracker grid period gets structure:
///
/// - *today*: one entry per hour covering every tracker kind and bin edge.
///   Interval trackers (sleep/temperature/water 1-day, run_times/notes
///   1-week) keep **at most one value per slot** — re-logging the same
///   tracker in the same slot replaces the previous entry, so the bin
///   samples are spread across distinct day/week slots;
/// - *yesterday / day-before*: the remaining temperature and sleep bins;
/// - *last week*: the inverted-`run_times` "worse" sample in its own slot;
/// - *rolling month fill*: one sleep + mood per day for 29 days back
///   (covers every alignment of the rolling month window, which starts
///   21..27 days back);
/// - *calendar-year fill*: one steps + mood per day from Jan 1 through
///   today, nearly filling the year heatmap and the `:year steps` rows.
async fn seed_mood_entries(
    pool: &sqlx::SqlitePool,
    config: &Config,
    today: i64,
    yesterday: i64,
) -> Result<()> {
    let h = |hour: i64| today + hour * 3600;
    let yh = |hour: i64| yesterday + hour * 3600;
    let two_days_ago = date::day_start(today - 2 * 86400);

    // --- today: tracker-kind + bin-edge coverage ---------------------------
    seed_entry(pool, config, "happy", "", h(9), &[]).await?; // mood only
    seed_entry(pool, config, "reflective", "Journal entry body", h(10), &[]).await?; // + body
    seed_entry(pool, config, "great", "", h(11), &[("sleep", "8")]).await?; // float, in range
    seed_entry(pool, config, "chilly", "", h(12), &[("temperature", "5")]).await?; // below min
                                                                                   // run_times is inverted (min 100, max 0): smaller is better → last color.
    seed_entry(pool, config, "energized", "", h(14), &[("run_times", "10")]).await?;
    seed_entry(pool, config, "hydrated", "", h(15), &[("water", "3")]).await?; // no min/max
    seed_entry(pool, config, "productive", "", h(16), &[("steps", "8000")]).await?; // number
    seed_entry(pool, config, "content", "", h(17), &[("mood_notes", "1")]).await?; // text ×3
    seed_entry(
        pool,
        config,
        "content",
        "",
        h(17) + 1800,
        &[("mood_notes", "2")],
    )
    .await?;
    seed_entry(pool, config, "content", "", h(18), &[("mood_notes", "3")]).await?;
    seed_entry(pool, config, "happy", "", h(21), &[]).await?; // same mood twice in one day
    seed_entry(pool, config, "", "End of day journal entry", h(22), &[]).await?; // journal-only
                                                                                 // Interval-`notes` replace demo (both in the current week slot): the
                                                                                 // later entry replaces the earlier one inside `create_entry`.
    seed_entry(
        pool,
        config,
        "reflective",
        "",
        h(8),
        &[("notes", "morning thoughts")],
    )
    .await?;
    seed_entry(
        pool,
        config,
        "reflective",
        "",
        h(23),
        &[("notes", "evening reflection")],
    )
    .await?;

    // --- yesterday ---------------------------------------------------------
    seed_entry(pool, config, "productive", "", yh(9), &[("steps", "3000")]).await?; // lower bin
    seed_entry(
        pool,
        config,
        "neutral",
        "",
        yh(12),
        &[("temperature", "20")],
    )
    .await?; // in range
    seed_entry(pool, config, "", "", yh(16), &[("steps", "5000")]).await?; // tracker-only, no mood
                                                                           // Multi-tracker entry. Timed at noon: replacement slots are a uniform
                                                                           // epoch-aligned grid (see `handlers::interval_slot`), so on UTC-offset
                                                                           // machines a late-evening entry can share a UTC-day slot with the next
                                                                           // morning's sample and silently replace it. Noon keeps every sample a
                                                                           // UTC-day apart.
    seed_entry(
        pool,
        config,
        "tired",
        "",
        yh(12) + 30 * 60,
        &[("sleep", "6"), ("water", "2")],
    )
    .await?;

    // --- two days ago ------------------------------------------------------
    seed_entry(
        pool,
        config,
        "hot",
        "",
        two_days_ago + 12 * 3600,
        &[("temperature", "35")],
    )
    .await?; // above max

    // --- last week ---------------------------------------------------------
    let last_week = date::day_start(today - 7 * 86400);
    // run_times inverted: larger is worse → first color, in its own week slot
    // so it isn't replaced by today's better sample.
    seed_entry(
        pool,
        config,
        "tired",
        "",
        last_week + 14 * 3600,
        &[("run_times", "90")],
    )
    .await?;

    // --- rolling month fill ------------------------------------------------
    // One sleep + mood per day for 29 days back, a few days skipped so the
    // grid reads as "almost" full (yesterday is skipped — it already carries
    // a sleep sample). Covers any rolling-month-window alignment.
    const MONTH_MOODS: &[&str] = &[
        "happy",
        "great",
        "neutral",
        "tired",
        "energized",
        "content",
        "hydrated",
    ];
    for i in 2..=29 {
        if i % 10 == 0 {
            continue; // leave a few ◯ cells
        }
        let t = date::day_start(today - i * 86400) + 10 * 3600;
        let mood = MONTH_MOODS[(i as usize) % MONTH_MOODS.len()];
        let sleep_val = format!("{:.1}", 6.5 + ((i % 6) as f64) * 0.5);
        seed_entry(pool, config, mood, "", t, &[("sleep", &sleep_val)]).await?;
    }

    // --- calendar-year fill ------------------------------------------------
    // One entry per day from Jan 1 through yesterday: nearly fills the year
    // heatmap and the `:year steps` rows.
    const YEAR_MOODS: &[&str] = &[
        "happy",
        "sad",
        "drained",
        "charged",
        "passive",
        "purposeful",
    ];
    const YEAR_STEPS: &[&str] = &["4000", "6000", "8000", "10000", "12000"];
    let mut t = date::day_start(date::year_start());
    let mut d = 0usize;
    while t < today {
        if !d.is_multiple_of(12) {
            let mood = YEAR_MOODS[d % YEAR_MOODS.len()];
            let steps = YEAR_STEPS[d % YEAR_STEPS.len()];
            seed_entry(pool, config, mood, "", t + 10 * 3600, &[("steps", steps)]).await?;
        }
        t = date::day_start(t + 86400);
        d += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Oneshot tasks
// ---------------------------------------------------------------------------

/// Oneshot tasks (interval None, availability None): covers no-date, today,
/// recent past, old past with a body, completed, partial multi-count,
/// priority extremes, optional, and future.
async fn seed_oneshot_tasks(
    pool: &sqlx::SqlitePool,
    now: i64,
    today: i64,
    yesterday: i64,
    tomorrow: i64,
) -> Result<()> {
    // No date (start = now).
    seed_task(
        pool,
        "buy groceries",
        "",
        5,
        Some(now),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    // Today at 9:00.
    seed_task(
        pool,
        "meeting",
        "",
        5,
        Some(today + 9 * 3600),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    // Recent past (yesterday) — shows up in @due.
    seed_task(
        pool,
        "dentist appointment",
        "",
        5,
        Some(yesterday + 10 * 3600),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    // Old past with a body (specific datetime).
    let old = date::parse_datetime("2024-03-20 14:30:00", date::DATE_DIALECT)?;
    seed_task(
        pool,
        "team sync",
        "sync notes",
        5,
        Some(old),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    // Completed oneshot (completion entry of 1).
    let id = seed_task(
        pool,
        "finish report",
        "",
        5,
        Some(yesterday + 15 * 3600),
        None,
        None,
        1,
        false,
        None,
    )
    .await?;
    update_task(pool, id, 1).await?;
    // Partial multi-count oneshot: target 3 with one completion (● 1/3 badge).
    let id = seed_task(
        pool,
        "launch checklist",
        "",
        5,
        Some(today + 8 * 3600),
        None,
        None,
        3,
        false,
        None,
    )
    .await?;
    update_task(pool, id, 1).await?;
    // Priority extremes (color binning).
    seed_task(
        pool,
        "urgent deploy",
        "",
        9,
        Some(today + 10 * 3600),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    seed_task(
        pool,
        "someday read",
        "",
        1,
        Some(tomorrow + 9 * 3600),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;
    // Optional task.
    seed_task(
        pool,
        "stretch",
        "",
        5,
        Some(today + 12 * 3600),
        None,
        None,
        0,
        true,
        None,
    )
    .await?;
    // Future (tomorrow) — not yet due.
    seed_task(
        pool,
        "plan trip",
        "",
        5,
        Some(tomorrow + 9 * 3600),
        None,
        None,
        0,
        false,
        None,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Recurring tasks
// ---------------------------------------------------------------------------

/// Recurring tasks (interval set, availability optional): covers plain
/// daily, weekly-with-availability, end_time, target count, and body text.
async fn seed_recurring_tasks(pool: &sqlx::SqlitePool, now: i64) -> Result<()> {
    // Daily, no availability window; 3 completions in the current interval.
    let id = seed_task(
        pool,
        "recurring_1",
        "",
        5,
        Some(now),
        None,
        Some(86400),
        0,
        false,
        None,
    )
    .await?;
    update_task(pool, id, 3).await?;
    // Weekly with a 2-day availability window (recurring + availability).
    seed_task(
        pool,
        "recurring_2",
        "weekly review",
        7,
        Some(now),
        Some(2 * 86400),
        Some(7 * 86400),
        0,
        false,
        None,
    )
    .await?;
    // Daily, stops after 3 days (end_time).
    seed_task(
        pool,
        "recurring_3",
        "",
        5,
        Some(now),
        None,
        Some(86400),
        0,
        false,
        Some(now + 3 * 86400),
    )
    .await?;
    // Daily with a target of 5, optional.
    seed_task(
        pool,
        "recurring_4",
        "",
        5,
        Some(now),
        None,
        Some(86400),
        5,
        true,
        None,
    )
    .await?;
    // Daily with a body.
    seed_task(
        pool,
        "recurring_5",
        "standup notes",
        5,
        Some(now),
        None,
        Some(86400),
        0,
        false,
        None,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 4. Scheduled tasks
// ---------------------------------------------------------------------------

/// Scheduled tasks (interval None, availability Some): covers ongoing,
/// future-today, elapsed auto-completed, completed early (entry 1), failed
/// (entry 0), and fully-past (discoverable in @due).
async fn seed_scheduled_tasks(
    pool: &sqlx::SqlitePool,
    now: i64,
    today: i64,
    yesterday: i64,
) -> Result<()> {
    // Ongoing: window not elapsed, no completion entry.
    seed_task(
        pool,
        "meditate",
        "",
        10,
        Some(now - 30 * 60),
        Some(2 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;
    // Future today: window overlaps today (start < today_end ∧ start+dur > today_start).
    seed_task(
        pool,
        "call mom",
        "",
        10,
        Some(today + 18 * 3600),
        Some(3600),
        None,
        0,
        false,
        None,
    )
    .await?;
    // Elapsed, no entry → auto-completed (shows in @done without include_completed).
    seed_task(
        pool,
        "clean desk",
        "",
        10,
        Some(now - 3 * 3600),
        Some(3600),
        None,
        0,
        false,
        None,
    )
    .await?;
    // Completed early: explicit entry of 1.
    let id = seed_task(
        pool,
        "write blog",
        "",
        10,
        Some(now - 30 * 60),
        Some(2 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;
    set_scheduled_completion(pool, id, 1).await?;
    // Failed: explicit entry of 0.
    let id = seed_task(
        pool,
        "pay bills",
        "",
        10,
        Some(now - 30 * 60),
        Some(2 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;
    set_scheduled_completion(pool, id, 0).await?;
    // Fully past (yesterday) — listed in @due (start < today_end).
    seed_task(
        pool,
        "water plants",
        "",
        10,
        Some(yesterday + 10 * 3600),
        Some(3600),
        None,
        0,
        false,
        None,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a raw tracker value into the typed payload per the config's
/// declared kind (mirrors `handlers::parse_tracker_value`; the seed script
/// does not go through the CLI).
fn tracker_value(config: &Config, name: &str, raw: &str) -> Result<TrackerValue> {
    let tracker = config
        .tracker
        .get(name)
        .with_context(|| format!("Unknown tracker type '{name}' not found in config"))?;
    Ok(match tracker.kind {
        TrackerKind::Text => TrackerValue::Text(raw.to_string()),
        TrackerKind::Integer => {
            TrackerValue::Integer(raw.parse().with_context(|| {
                format!("Cannot parse '{raw}' as an integer for tracker '{name}'")
            })?)
        }
        TrackerKind::Float => {
            TrackerValue::Float(raw.parse().with_context(|| {
                format!("Cannot parse '{raw}' as a number for tracker '{name}'")
            })?)
        }
        // Duration values are duration strings stored as seconds.
        TrackerKind::Duration => {
            let secs = humantime::parse_duration(raw).with_context(|| {
                format!("Cannot parse '{raw}' as a duration for tracker '{name}'")
            })?;
            TrackerValue::Float(secs.as_secs_f64())
        }
        // Null trackers don't take values; the seed script never seeds them.
        TrackerKind::Null => anyhow::bail!("Null tracker '{name}' cannot be seeded with a value"),
    })
}

/// Interval slot whose previous entry gets replaced on insert, for
/// Text/Float trackers with an interval. Mirrors the calendar slot math in
/// `commands::entry`: `[anchor + span*k, anchor + span*(k+1))`.
fn replace_slot(config: &Config, name: &str, time: i64) -> Option<(i64, i64)> {
    config
        .tracker
        .get(name)
        .filter(|t| t.interval.is_some_and(|iv| !iv.cumulative))
        .and_then(|t| t.interval)
        .and_then(|iv| crate::date::interval_slot_unix_secs(iv.anchor, iv.span, time))
}

/// Insert one mood/tracker entry with `embedding: None`. The grid and today
/// views recompute embeddings on the fly and backfill them on first render.
async fn seed_entry(
    pool: &sqlx::SqlitePool,
    config: &Config,
    mood: &str,
    body: &str,
    time: i64,
    trackers: &[(&str, &str)],
) -> Result<()> {
    let trackers = trackers
        .iter()
        .map(|(name, raw)| -> Result<TrackerObject> {
            Ok(TrackerObject {
                tracker_type: name.to_string(),
                value: tracker_value(config, name, raw)?,
                replace_slot: replace_slot(config, name, time),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let entry = EntryObject {
        mood: mood.to_string(),
        body: body.to_string(),
        time,
        embedding: None,
        score: None,
        trackers,
    };
    create_entry(pool, &entry).await?;
    Ok(())
}

/// Insert a task via the typed API; returns its row id for later
/// completion inserts.
#[allow(clippy::too_many_arguments)]
async fn seed_task(
    pool: &sqlx::SqlitePool,
    name: &str,
    body: &str,
    priority: i32,
    start_time: Option<i64>,
    available_duration_secs: Option<i64>,
    interval_secs: Option<i64>,
    target_count: i32,
    optional: bool,
    end_time: Option<i64>,
) -> Result<i64> {
    let task = TaskObject {
        id: None,       // row id auto-assigned by the db layer
        short_id: None, // short id allocated by the db layer
        name: name.to_string(),
        body: body.to_string(),
        priority,
        start_time,
        available_duration_secs,
        interval_secs,
        target_count,
        optional,
        end_time,
        parent: None,
    };
    let (id, _) = create_task(pool, &task).await?;
    Ok(id)
}
