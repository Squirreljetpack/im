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
//! Entries are spread across time frames — today, yesterday,
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
    create_entry, create_task, update_task, EntryObject, TaskObject, TrackerObject, TrackerValue,
};

fn main() -> Result<()> {
    let db_path = std::env::args()
        .nth(1)
        .context("Usage: seed-db <DB_PATH>")?;

    // Load config from IM_CONFIG_DIR if set, else fallback to bundled dev.toml
    let config: Config = if let Some(dir) = std::env::var_os("IM_CONFIG_DIR") {
        let dir_path = PathBuf::from(dir);
        let p_dev = dir_path.join("dev.toml");
        let p_cfg = dir_path.join("config.toml");
        let p = if p_dev.exists() { p_dev } else { p_cfg };
        if p.exists() {
            let s = std::fs::read_to_string(&p)
                .with_context(|| format!("failed to read config at {}", p.display()))?;
            toml::from_str(&s)
                .with_context(|| format!("failed to parse config at {}", p.display()))?
        } else {
            toml::from_str(include_str!("../assets/dev.toml"))
                .context("assets/dev.toml must parse into Config")?
        }
    } else {
        toml::from_str(include_str!("../assets/dev.toml"))
            .context("assets/dev.toml must parse into Config")?
    };

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
    let yesterday = date::shift_days(today, -1);
    let tomorrow = date::shift_days(today, 1);

    // 1. Mood & tracker entries (embedding: None everywhere).
    seed_mood_entries(pool, config, today, yesterday).await?;

    // 2. Tasks: oneshot, recurring, and scheduled.
    // Created in deliberate order to ensure short_ids match canonical expectations (e.g. fist docs = id 7).
    seed_tasks(pool, now, today, yesterday, tomorrow).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Mood & tracker entries
// ---------------------------------------------------------------------------

async fn seed_mood_entries(
    pool: &sqlx::SqlitePool,
    config: &Config,
    today: i64,
    yesterday: i64,
) -> Result<()> {
    let yh = |hour: i64| yesterday + hour * 3600;
    let two_days_ago = date::shift_days(today, -2);

    // --- today: exactly matches .README.assets/image-20260814134534039.png ---
    // 01:54 ● run
    // 01:54 ◆ win: yes
    // 01:57 ◆ sleep: 8-14 01:57
    let t_0154 = today + 1 * 3600 + 54 * 60;
    let t_0157 = today + 1 * 3600 + 57 * 60;
    seed_entry(pool, config, "run", "", t_0154, &[]).await?;
    seed_entry(pool, config, "", "", t_0154, &[("win", "yes")]).await?;
    seed_entry(pool, config, "", "", t_0157, &[("sleep", "8-14 01:57")]).await?;

    // --- yesterday & recent past: tracker-kind + bin-edge coverage -----------
    seed_entry(pool, config, "purposeful", "", yh(9), &[("steps", "3000")]).await?;
    seed_entry(pool, config, "", "", yh(12), &[("temperature", "20")]).await?;
    seed_entry(pool, config, "", "", yh(16), &[("steps", "5000")]).await?;
    seed_entry(
        pool,
        config,
        "",
        "",
        yh(12) + 30 * 60,
        &[("water", "2")],
    )
    .await?;

    // --- two days ago ------------------------------------------------------
    seed_entry(
        pool,
        config,
        "comfortable",
        "",
        two_days_ago + 12 * 3600,
        &[("temperature", "35")],
    )
    .await?;

    // --- last week ---------------------------------------------------------
    let last_week = date::shift_days(today, -7);
    seed_entry(
        pool,
        config,
        "tired",
        "",
        last_week + 14 * 3600,
        &[("run_times", "90")],
    )
    .await?;

    // --- rolling month fill: bones & cats_defeated ------------------------
    // cats_defeated is max 1 on any given day, bones is max 10 on any given day.
    // The days cats_defeated are recorded (5 of them) are precisely the days that bones is not.
    if config.tracker.contains_key("bones") && config.tracker.contains_key("cats_defeated") {
        let rolling_start = date::rolling_month_start(jiff::civil::Weekday::Monday);
        let mut day_cur = rolling_start;
        let mut days = Vec::new();
        while day_cur < today {
            days.push(day_cur);
            day_cur = date::shift_days(day_cur, 1);
        }
        let cat_indices: [usize; 5] = if days.len() >= 27 {
            [4, 9, 11, 12, 23]
        } else {
            let n = days.len();
            [n / 6, 2 * n / 6, 3 * n / 6, 4 * n / 6, 5 * n / 6]
        };

        for (idx, &day_ts) in days.iter().enumerate() {
            let t = day_ts + 12 * 3600;
            if cat_indices.contains(&idx) {
                seed_entry(pool, config, "", "", t, &[("cats_defeated", "1")]).await?;
            } else {
                let bone_val = format!("{}", ((idx * 3) % 7) + 4);
                seed_entry(pool, config, "", "", t, &[("bones", &bone_val)]).await?;
            }
        }
    }

    // --- calendar-year fill: single pre-configured mood per day -----------
    // Uses a small, curated set of pre-configured moods with organic multi-day
    // clusters so each day displays a clean, harmonized dot color.
    const YEAR_MOOD_VOCAB: &[&str] = &[
        "happy",
        "sad",
        "drained",
        "energetic",
        "thoughtful",
        "purposeful",
        "passive",
        "frustrated",
        "neutral",
        "nostalgic",
        "humble",
        "secure",
        "pained",
        "comfortable",
        "guarded",
        "vulnerable",
        "isolated",
        "sociable",
        "connected",
        "confused",
        "lucid",
        "proud",
        "let down",
        "engrossed",
        "disgusted",
        "satiated",
        "hungry",
        "bereaved",
        "vengeful",
        "fearful",
        "smitten",
        "optimistic",
        "envious",
    ];

    let mut t = date::day_start(date::year_start());
    let mut state: u64 = 0x9e3779b97f4a7c15;
    let mut next_u32 = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 32) as u32
    };

    let mut unvisited: Vec<usize> = (0..YEAR_MOOD_VOCAB.len()).collect();
    let mut current_mood_idx = 0usize;
    let mut streak_remaining = 0usize;

    while t < today {
        let r = next_u32();
        // Occasional skipped days (~6% of days) for natural texture
        if (r % 16) != 0 {
            if streak_remaining == 0 {
                if !unvisited.is_empty() {
                    let pick = (r as usize) % unvisited.len();
                    current_mood_idx = unvisited.swap_remove(pick);
                } else {
                    current_mood_idx = (r as usize) % YEAR_MOOD_VOCAB.len();
                }
                streak_remaining = 1 + ((r >> 4) as usize % 3); // 1 to 3 days streak
            } else {
                streak_remaining -= 1;
            }

            let mood = YEAR_MOOD_VOCAB[current_mood_idx];
            let steps_base = 4000 + ((r >> 8) % 9000); // 4000 to 13000
            let steps = steps_base.to_string();
            seed_entry(pool, config, mood, "", t + 10 * 3600, &[("steps", &steps)]).await?;
        } else {
            streak_remaining = 0;
        }
        t = date::shift_days(t, 1);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Tasks (oneshot, recurring, scheduled)
// ---------------------------------------------------------------------------

async fn seed_tasks(
    pool: &sqlx::SqlitePool,
    _now: i64,
    today: i64,
    yesterday: i64,
    tomorrow: i64,
) -> Result<()> {
    // We create tasks in precise sequence to allocate short_ids predictably:
    // 1: update im ui
    // 2: missing colors, render method for im, full tui test
    // 3: wordle (the 1 extra recurring task active today!)
    // 4: pushups (recurring, daily)
    // 5: weekly review (recurring, weekly)
    // 6: write 30m
    // 7: fist docs (priority 10, creation: 2026-08-13 19:27)

    // Task 1: update im ui (completed today at 00:49)
    let t_0049 = today + 49 * 60;
    let t_create_1 = date::parse_datetime("2026-08-13 18:00:00", date::DATE_DIALECT)?;
    let id1 = seed_task(pool, "update im ui", "", 5, t_create_1, None, None, 1, false, None).await?;
    update_task(pool, id1, 1).await?;
    sqlx::query("UPDATE todo_completions SET time = ? WHERE todo_id = ?")
        .bind(t_0049)
        .bind(id1)
        .execute(pool)
        .await?;

    // Task 2: missing colors, render method for im, full tui test (completed today at 00:43)
    let t_0043 = today + 43 * 60;
    let t_create_2 = date::parse_datetime("2026-08-13 17:00:00", date::DATE_DIALECT)?;
    let id2 = seed_task(
        pool,
        "missing colors, render method for im, full tui test",
        "",
        5,
        t_create_2,
        None,
        None,
        1,
        false,
        None,
    )
    .await?;
    update_task(pool, id2, 1).await?;
    sqlx::query("UPDATE todo_completions SET time = ? WHERE todo_id = ?")
        .bind(t_0043)
        .bind(id2)
        .execute(pool)
        .await?;

    // Task 3: wordle — THE 1 EXTRA RECURRING TASK (active today!)
    // Interval: 1 day, open availability window today, 0 completions. Shows as `↻   wordle`.
    seed_task(
        pool,
        "wordle",
        "",
        5,
        today,
        None,
        Some(86400),
        0,
        false,
        None,
    )
    .await?;

    // Task 4: pushups (recurring, interval: 1 day, starts tomorrow so does not show in Today)
    seed_task(
        pool,
        "pushups",
        "",
        5,
        tomorrow,
        None,
        Some(86400),
        0,
        false,
        None,
    )
    .await?;

    // Task 5: weekly review (recurring, weekly with 2-day weekend window, starts tomorrow)
    seed_task(
        pool,
        "weekly review",
        "weekly reflection and planning",
        7,
        tomorrow,
        Some(2 * 86400),
        Some(7 * 86400),
        0,
        false,
        None,
    )
    .await?;

    // Task 6: write 30m (untimed oneshot)
    let t_create_write = date::parse_datetime("2026-08-13 18:30:00", date::DATE_DIALECT)?;
    seed_task(pool, "write 30m", "", 5, t_create_write, None, None, 0, false, None).await?;

    // Task 7: fist docs (priority 10, creation: 2026-08-13 19:27)
    // Matches .README.assets/image-20260814134534039.png preview pane exactly!
    let t_create_fist = date::parse_datetime("2026-08-13 19:27:00", date::DATE_DIALECT)?;
    let id_fist = seed_task(pool, "fist docs", "", 10, t_create_fist, None, None, 0, false, None).await?;
    let _ = im::db::update_task_short_id(pool, id_fist, 7).await;

    // Task 8: test mmclipboard
    let t_create_mm = date::parse_datetime("2026-08-13 19:35:00", date::DATE_DIALECT)?;
    seed_task(pool, "test mmclipboard", "", 5, t_create_mm, None, None, 0, false, None).await?;

    // Task 9: read im reload code
    let t_create_reload = date::parse_datetime("2026-08-13 20:00:00", date::DATE_DIALECT)?;
    seed_task(pool, "read im reload code", "", 5, t_create_reload, None, None, 0, false, None).await?;

    // Task 10: use pickerui for menu
    let t_create_picker = date::parse_datetime("2026-08-13 20:15:00", date::DATE_DIALECT)?;
    seed_task(pool, "use pickerui for menu", "", 5, t_create_picker, None, None, 0, false, None).await?;

    // Task 11: test fist menu actions
    let t_create_menu = date::parse_datetime("2026-08-13 20:30:00", date::DATE_DIALECT)?;
    seed_task(pool, "test fist menu actions", "", 5, t_create_menu, None, None, 0, false, None).await?;

    // Task 12: figure out fist download check api shape
    let t_create_dl = date::parse_datetime("2026-08-13 21:00:00", date::DATE_DIALECT)?;
    seed_task(pool, "figure out fist download check api shape", "", 5, t_create_dl, None, None, 0, false, None).await?;

    // Task 13: understand and rework aliases for fzs to be stored
    let t_create_aliases = date::parse_datetime("2026-08-13 21:30:00", date::DATE_DIALECT)?;
    seed_task(pool, "understand and rework aliases for fzs to be stored", "", 5, t_create_aliases, None, None, 0, false, None).await?;

    // --- Tasks for @due:w and tasks view (@, @:o, @:O) ---------------------
    // Scheduled tasks starting tomorrow or later (within the week horizon):
    // They do NOT appear in today's view (start_time >= today + 86400),
    // but DO appear in @due:w and @:O!

    // Scheduled for tomorrow (shows in @due:w and @:O)
    seed_task(
        pool,
        "prepare release notes",
        "",
        6,
        tomorrow + 14 * 3600,
        Some(2 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;

    // Scheduled for 3 days out (shows in @due:w and @:O)
    seed_task(
        pool,
        "quarterly roadmap review",
        "",
        7,
        today + 3 * 86400 + 10 * 3600,
        Some(2 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;

    // Scheduled for tomorrow (shows in @due:w and @:O)
    seed_task(
        pool,
        "dentist appointment",
        "",
        15,
        tomorrow + 10 * 3600,
        Some(3600),
        None,
        0,
        false,
        None,
    )
    .await?;

    // Scheduled task for 5 days out (shows in @:O)
    seed_task(
        pool,
        "flight to san francisco",
        "",
        15,
        today + 5 * 86400 + 8 * 3600,
        Some(6 * 3600),
        None,
        0,
        false,
        None,
    )
    .await?;

    // Additional interesting recurring tasks (shows in @:O)
    seed_task(
        pool,
        "deep clean apartment",
        "",
        5,
        tomorrow,
        None,
        Some(14 * 86400),
        0,
        true,
        None,
    )
    .await?;

    seed_task(
        pool,
        "duolingo spanish",
        "",
        5,
        tomorrow,
        None,
        Some(86400),
        5,
        true,
        None,
    )
    .await?;

    // Completed tasks for @done
    let id_done1 = seed_task(pool, "finish report", "", 5, yesterday + 15 * 3600, None, None, 1, false, None).await?;
    update_task(pool, id_done1, 1).await?;
    sqlx::query("UPDATE todo_completions SET time = ? WHERE todo_id = ?")
        .bind(yesterday + 15 * 3600)
        .bind(id_done1)
        .execute(pool)
        .await?;

    let old_ts = date::parse_datetime("2026-08-01 14:00:00", date::DATE_DIALECT)?;
    let id_done2 = seed_task(pool, "team sync", "sync notes", 5, old_ts, None, None, 1, false, None).await?;
    update_task(pool, id_done2, 1).await?;
    sqlx::query("UPDATE todo_completions SET time = ? WHERE todo_id = ?")
        .bind(old_ts + 3600)
        .bind(id_done2)
        .execute(pool)
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
        TrackerKind::Duration => {
            let secs = humantime::parse_duration(raw).with_context(|| {
                format!("Cannot parse '{raw}' as a duration for tracker '{name}'")
            })?;
            TrackerValue::Float(secs.as_secs_f64())
        }
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
        .and_then(|iv| iv.slot_for(im::date::LogTime::local(time)))
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
        time: im::date::LogTime::local(time),
        location: im::date::current_location(),
        embedding: None,
        score: None,
        trackers,
        duration: None,
        todo_id: None,
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
    start_time: i64,
    available_duration_secs: Option<i64>,
    interval_secs: Option<i64>,
    target_count: i32,
    optional: bool,
    end_time: Option<i64>,
) -> Result<im::db::Id> {
    let task = TaskObject {
        id: None,       // row id assigned by the db layer
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
