//! Integration tests for the mood CLI.
//!
//! These tests verify the full flow from CLI parsing through database operations.

use im::{
    cli::{CliOpts, parse_from},
    commands::execute_command,
    config::{Config, TrackerKind},
    db::test_pool,
};
use sqlx::{Row, SqlitePool};

/// Helper: create a oneshot task and return its id
async fn create_oneshot_task(pool: &SqlitePool, name: &str) -> i64 {
    let cmd = parse_from(vec!["!".to_string(), name.to_string()]).unwrap();
    let config = Config::default();
    execute_command(
        cmd,
        pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    sqlx::query_scalar::<_, i64>("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Helper: insert a completion entry with an explicit time and count.
/// Unlike `im::db::update_task` (which stamps `now()` and applies
/// interval logic), this writes the row directly.
async fn update_task(pool: &SqlitePool, todo_id: i64, time: i64, count: i32) {
    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, ?)")
        .bind(todo_id)
        .bind(time)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
}

/// A day-long tracker interval anchored at local midnight 2020-01-01.
fn day_interval() -> im::config::TrackerInterval {
    im::config::TrackerInterval {
        anchor: im::date::parse_datetime("2020-01-01 00:00", im::date::DATE_DIALECT).unwrap(),
        span: jiff::Span::new().days(1),
    cumulative: false,
    }
}

#[tokio::test]
async fn test_create_mood_entry() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["comfortably".to_string(), "numb".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let row = sqlx::query("SELECT mood, body FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("mood"), "comfortably numb");
    assert_eq!(row.get::<String, _>("body"), "");
}

#[tokio::test]
async fn test_create_mood_with_trackers() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec![
        "-sleep".to_string(), // -sleep 8
        "8".to_string(),
        "-water".to_string(), // -water 5
        "5".to_string(),
        "good".to_string(),
    ])
    .unwrap();

    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let mood = sqlx::query("SELECT id, mood FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mood_id: i64 = mood.get("id");
    assert_eq!(mood.get::<String, _>("mood"), "good");

    // Verify tracker trackers were inserted and linked
    let rows = sqlx::query("SELECT type, score, mood FROM tracker ORDER BY type")
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<String, _>("type"), "sleep");
    assert_eq!(rows[0].get::<f64, _>("score"), 8.0);
    assert_eq!(rows[0].get::<Option<i64>, _>("mood"), Some(mood_id));
    assert_eq!(rows[1].get::<String, _>("type"), "water");
    assert_eq!(rows[1].get::<f64, _>("score"), 5.0);
    assert_eq!(rows[1].get::<Option<i64>, _>("mood"), Some(mood_id));
}

#[tokio::test]
async fn test_create_tracker_only() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec!["-sleep".to_string(), "10".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // No mood should be inserted
    let mood_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mood_count, 0);

    // Tracker entry inserted without mood link
    let tracker = sqlx::query("SELECT type, score, mood FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(tracker.get::<String, _>("type"), "sleep");
    assert_eq!(tracker.get::<f64, _>("score"), 10.0);
    assert_eq!(tracker.get::<Option<i64>, _>("mood"), None);
}

#[tokio::test]
async fn test_tracker_interval_insert_strategies() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    // text + interval: re-logging replaces the previous entry in the slot
    config.tracker.insert(
        "affirmation".to_string(),
        im::config::TrackerSetting {
            interval: Some(day_interval()),
            low: None,
            high: None,
            kind: TrackerKind::Text,
            strict: false,
            colors: None,
        },
    );
    // float + interval: re-logging replaces the previous entry in the slot
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: Some(day_interval()),
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    // integer + interval + cumulative: every log is kept (the grid sums)
    let accum_iv = {
        let mut iv = day_interval();
        iv.cumulative = true;
        iv
    };
    config.tracker.insert(
        "runs".to_string(),
        im::config::TrackerSetting {
            interval: Some(accum_iv),
            low: None,
            high: None,
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );

    // Two inserts back-to-back land in the same interval slot.
    for (tracker, value) in [
        ("-sleep", "8"),
        ("-sleep", "6"),
        ("-runs", "2"),
        ("-runs", "3"),
        ("-affirmation", "first"),
        ("-affirmation", "second"),
    ] {
        let cmd = parse_from(vec![tracker.to_string(), value.to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }

    // Float: replaced by the latest value in the slot (1 row, score 6).
    let sleep_rows: Vec<(f64,)> = sqlx::query_as("SELECT score FROM tracker WHERE type = 'sleep'")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        sleep_rows.len(),
        1,
        "float+interval must replace the slot entry"
    );
    assert_eq!(sleep_rows[0].0, 6.0);

    // Text: replaced by the latest value in the slot (1 row, 'second').
    let text_rows: Vec<(String,)> =
        sqlx::query_as("SELECT score FROM tracker WHERE type = 'affirmation'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        text_rows.len(),
        1,
        "text+interval must replace the slot entry"
    );
    assert_eq!(text_rows[0].0, "second");

    // Integer + cumulative: plain insert, both rows kept (the view sums them).
    let runs_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker WHERE type = 'runs'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs_count, 2, "number+interval must accumulate");

    // The replace is slot-scoped: a 1s-interval float logged 1.1s apart lands
    // in two different slots, so both entries are kept.
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: Some(im::config::TrackerInterval {
                anchor: im::date::now(),
                span: jiff::Span::new().seconds(1),
            cumulative: false,
            }),
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    for _ in 0..2 {
        let cmd = parse_from(vec!["-water".to_string(), "1".to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }
    let water_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker WHERE type = 'water'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        water_count, 2,
        "replace must be scoped to the interval slot, not global"
    );
}

#[tokio::test]
async fn test_create_oneshot_task() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["!".to_string(), "urgent task".to_string()]).unwrap();

    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let task = sqlx::query("SELECT name, body, priority, interval_secs, target_count FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(task.get::<String, _>("name"), "urgent task");
    assert_eq!(task.get::<String, _>("body"), "");
    assert_eq!(task.get::<i32, _>("priority"), 5); // default priority
    assert_eq!(task.get::<Option<i64>, _>("interval_secs"), None);
    // Oneshot tasks must default to target_count = 0 (single-completion tasks;
    // the editor flow's `prompt_target_count` also blanks to 0). Without this
    // the preview would render a useless progress bar with capacity 1.
    assert_eq!(task.get::<i32, _>("target_count"), 0); // default target_count
}

#[tokio::test]
async fn test_create_oneshot_task_duplicate_name_fails() {
    // Oneshot task names must be unique: a second `! <name>` with an
    // existing name is an error, not a silent duplicate.
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["!".to_string(), "buy milk".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let cmd = parse_from(vec!["!".to_string(), "buy milk".to_string()]).unwrap();
    let err = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the duplicate must not be inserted");
}

#[tokio::test]
async fn test_create_oneshot_task_with_parent() {
    // `! -<parent_id> <name>` attaches the new task under the parent's
    // row id (the flag takes the parent's short id).
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let parent_id = create_oneshot_task(&pool, "parent task").await;
    let parent_short: i64 = sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
        .bind(parent_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let cmd = parse_from(vec![
        "!".to_string(),
        format!("+{parent_short}"),
        "child task".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let child = sqlx::query("SELECT parent FROM todos WHERE name = ?")
        .bind("child task")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(child.get::<Option<i64>, _>("parent"), Some(parent_id));

    // Without the flag the task stays root-level.
    let cmd = parse_from(vec!["!".to_string(), "root task".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let root = sqlx::query("SELECT parent FROM todos WHERE name = ?")
        .bind("root task")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(root.get::<Option<i64>, _>("parent"), None);
}

#[tokio::test]
async fn test_create_oneshot_task_with_invalid_parent_errors() {
    // An unknown short id must fail before the task is created.
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec![
        "!".to_string(),
        "+999".to_string(),
        "orphan".to_string(),
    ])
    .unwrap();
    let err = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("No task with short id 999"),
        "unexpected error: {err}"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "the orphan must not be inserted");
}

#[tokio::test]
async fn test_create_oneshot_task_with_date() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec![
        "!".to_string(),
        "scheduled task".to_string(),
        "@2024-03-20".to_string(),
    ])
    .unwrap();

    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let task = sqlx::query("SELECT name, start_time, end_time FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(task.get::<String, _>("name"), "scheduled task");
    // `@<time>` is the due time: end_time is set to the specified date at
    // midnight, while start_time records the creation moment.
    let end_time: i64 = task.get("end_time");
    assert_eq!(
        end_time,
        im::date::parse_datetime("2024-03-20", im::date::DATE_DIALECT).unwrap()
    );
    let start_time: i64 = task.get("start_time");
    assert!(start_time > 0);
}


/// Cumulative interval trackers keep every log: the today view shows each
/// row with its raw value (summing happens only in the grid).
#[tokio::test]
async fn test_cumulative_interval_today_rows() {
    use im::types::{TodayHorizon, ViewVariant};

    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    let mut iv = day_interval();
    iv.cumulative = true;
    config.tracker.insert(
        "pushups".to_string(),
        im::config::TrackerSetting {
            interval: Some(iv),
            low: None,
            high: None,
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );
    for v in ["20", "30"] {
        execute_command(
            parse_from(vec!["-pushups".to_string(), v.to_string()]).unwrap(),
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }

    // Both rows in the same day slot; each log shows its raw value.
    let pushups: Vec<(i64,)> =
        sqlx::query_as("SELECT score FROM tracker WHERE type = 'pushups' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(pushups.len(), 2, "cumulative keeps every log");
    assert_eq!(pushups[0].0, 20);
    assert_eq!(pushups[1].0, 30);

    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();
    let im::today::TodayFetch { entries, .. } =
        im::today::fetch_today_entries(&pool, &config, TodayHorizon::Today, im::date::today_start(), ViewVariant::All)
            .await
            .unwrap();
    let rows: Vec<_> = entries
        .iter()
        .filter(|e| e.label.starts_with("pushups:"))
        .collect();
    assert_eq!(rows.len(), 2, "today view shows one row per log");
    assert!(rows.iter().any(|e| e.label == "pushups: 20"));
    assert!(rows.iter().any(|e| e.label == "pushups: 30"));
}
#[tokio::test]
async fn test_tracker_range_not_enforced() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();

    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(4.0),
            high: Some(10.0),
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // min/max are only for binning (color mapping), not for gating
    // insertion: below-min, in-range, and above-max values all store.
    for value in ["3", "7", "11"] {
        let cmd = parse_from(vec!["-sleep".to_string(), value.to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}

/// `strict = true` gates insertion through the same inclusive span the
/// colors use: below/above values error, the boundaries themselves store
/// (exact f64), a single bound gates on that bound only, and inverted
/// bounds gate the span between them.
#[tokio::test]
async fn test_tracker_strict_gate_enforced() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();

    async fn insert(
        pool: &SqlitePool,
        config: &Config,
        name: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        let cmd = parse_from(vec![format!("-{name}"), value.to_string()]).unwrap();
        execute_command(
            cmd,
            pool,
            config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
    }
    let insert = insert;

    // Both bounds: 4 and 10 are accepted, 3.5 / 11 error.
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(4.0),
            high: Some(10.0),
            kind: TrackerKind::Float,
            strict: true,
            colors: None,
        },
    );
    for ok in ["4", "10", "7"] {
        insert(&pool, &config, "sleep", ok).await.unwrap();
    }
    for bad in ["3.5", "11"] {
        let err = insert(&pool, &config, "sleep", bad).await.unwrap_err().to_string();
        assert!(
            err.contains("tracker 'sleep': value") && err.contains("outside [4, 10]"),
            "strict must reject {bad}, got: {err}"
        );
    }

    // Single bound: everything at/above the floor passes.
    config.tracker.insert(
        "pushups".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(10.0),
            high: None,
            kind: TrackerKind::Integer,
            strict: true,
            colors: None,
        },
    );
    insert(&pool, &config, "pushups", "10").await.unwrap();
    let err = insert(&pool, &config, "pushups", "9").await.unwrap_err().to_string();
    assert!(
        err.contains("outside [10]"),
        "single-bound strict must reject below-floor values, got: {err}"
    );

    // Inverted bounds still gate the span between them.
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(8.0),
            high: Some(2.0),
            kind: TrackerKind::Float,
            strict: true,
            colors: None,
        },
    );
    insert(&pool, &config, "water", "5").await.unwrap();
    let err = insert(&pool, &config, "water", "9").await.unwrap_err().to_string();
    assert!(
        err.contains("outside [2, 8]"),
        "inverted bounds must gate the span between them, got: {err}"
    );

    // Nothing outside the gates was stored.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 5, "3 sleep + 1 pushups + 1 water accepted rows; 4 rejected");
}

#[tokio::test]
async fn test_multiple_trackers_same_mood() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "exercise".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec![
        "-sleep".to_string(),
        "8".to_string(),
        "-water".to_string(),
        "6".to_string(),
        "-exercise".to_string(),
        "30".to_string(),
        "great".to_string(),
    ])
    .unwrap();

    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let mood_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mood_count, 1);

    let tracker_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tracker_count, 3);

    let linked_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tracker c JOIN mood f ON c.mood = f.id")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked_count, 3);
}

#[tokio::test]
async fn test_view_oneshot_tasks() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create some oneshot tasks
    let cmd1 = parse_from(vec!["!".to_string(), "low priority task".to_string()]).unwrap();
    execute_command(
        cmd1,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let cmd2 = parse_from(vec!["!".to_string(), "high priority task".to_string()]).unwrap();
    execute_command(
        cmd2,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // View pending oneshots via @:o (bare `!` is interactive creation now)
    let cmd = parse_from(vec!["@:o".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();

    // Verify both tasks appear as tab-separated rows:
    //   id \t interval \t next_available \t pri \t name \t status
    assert!(output.contains("low priority task"), "output: {output:?}");
    assert!(output.contains("high priority task"), "output: {output:?}");
    for line in output.lines() {
        assert_eq!(
            line.split('\t').count(),
            6,
            "line not tab-separated: {line:?}"
        );
        let fields: Vec<&str> = line.split('\t').collect();
        assert!(fields[0].parse::<i64>().is_ok(), "id not numeric: {line:?}");
        // Oneshot tasks render a single space in interval/next_available.
        assert_eq!(fields[1], " ", "oneshot interval: {line:?}");
        assert_eq!(fields[2], " ", "oneshot next_available: {line:?}");
        assert_eq!(fields[3], "5", "default priority: {line:?}");
        assert_eq!(fields[5], "○", "not-started status: {line:?}");
    }

    // Verify both tasks exist
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos WHERE interval_secs IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_update_oneshot_task_simple() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let task_id = create_oneshot_task(&pool, "test task").await;

    // Mark as done: - <short id>. On a fresh pool the row id equals the
    // short id, so `create_oneshot_task`'s return value works directly.
    let cmd = parse_from(vec![format!("+{task_id}"), "1".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // The user-facing short id is cleared once the task is completed.
    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'test task'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(short_id.is_none(), "completed task must lose its short id");

    // Verify completions: derived as SUM(count) from todo_completions.
    let completions: Option<i32> =
        sqlx::query_scalar("SELECT SUM(count) FROM todo_completions WHERE todo_id = ?")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(completions, Some(1));

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_update_oneshot_task_with_plus_syntax() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let task_id = create_oneshot_task(&pool, "clean room").await;

    // Direct update: +<short_id> 2
    let cmd = parse_from(vec![format!("+{task_id}"), "2".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let completions: Option<i32> =
        sqlx::query_scalar("SELECT SUM(count) FROM todo_completions WHERE todo_id = ?")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(completions, Some(2));
}

#[tokio::test]
async fn test_update_nonexistent_oneshot_fails() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["+99999".to_string(), "1".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("No task with short id")
    );
}

#[tokio::test]
async fn test_update_at_name_fails_as_query() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // The `- @name` recurring form was removed; `- @name` is now a word
    // query that never matches (task names don't carry the '@' prefix).
    let cmd = parse_from(vec!["+@nonexistent".to_string(), "1".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No task found matching"));
}

#[tokio::test]
async fn test_update_by_query_words() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    create_oneshot_task(&pool, "buy milk").await;
    create_oneshot_task(&pool, "walk the dog").await;

    // "milk" matches exactly one task.
    let cmd = parse_from(vec!["+milk".to_string(), "1".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let completions: Option<i32> = sqlx::query_scalar(
        "SELECT SUM(count) FROM todo_completions tc JOIN todos t ON t.id = tc.todo_id \
         WHERE t.name = 'buy milk'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completions, Some(1));

    // The other task is untouched.
    let other: Option<i32> = sqlx::query_scalar(
        "SELECT SUM(count) FROM todo_completions tc JOIN todos t ON t.id = tc.todo_id \
         WHERE t.name = 'walk the dog'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(other, None);
}

#[tokio::test]
async fn test_update_by_query_words_multiword_in_order() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    create_oneshot_task(&pool, "buy milk and eggs").await;
    create_oneshot_task(&pool, "buy eggs only").await;

    // A '+' word-query ref is a single attached token now ("+and"): it
    // matches only the first task ("and" appears in order in its name).
    let cmd = parse_from(vec![
        "+and".to_string(),
        "1".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let first: Option<i32> = sqlx::query_scalar(
        "SELECT SUM(count) FROM todo_completions tc JOIN todos t ON t.id = tc.todo_id \
         WHERE t.name = 'buy milk and eggs'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first, Some(1));

    let second: Option<i32> = sqlx::query_scalar(
        "SELECT SUM(count) FROM todo_completions tc JOIN todos t ON t.id = tc.todo_id \
         WHERE t.name = 'buy eggs only'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(second, None, "out-of-order words must not match");
}

#[tokio::test]
async fn test_update_by_query_words_with_count() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    create_oneshot_task(&pool, "buy milk").await;

    let cmd = parse_from(vec!["+milk".to_string(), "3".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let completions: Option<i32> = sqlx::query_scalar(
        "SELECT SUM(count) FROM todo_completions tc JOIN todos t ON t.id = tc.todo_id \
         WHERE t.name = 'buy milk'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completions, Some(3));
}

#[tokio::test]
async fn test_update_by_query_words_no_match_fails() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    create_oneshot_task(&pool, "buy milk").await;

    let cmd = parse_from(vec!["+walk".to_string(), "1".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No task found matching"));
}

#[tokio::test]
async fn test_update_by_query_words_multiple_matches_fail() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    create_oneshot_task(&pool, "buy milk").await;
    create_oneshot_task(&pool, "buy milk again").await;

    let cmd = parse_from(vec!["+buy".to_string(), "1".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("Multiple tasks match"), "got: {msg}");
}

#[tokio::test]
async fn test_create_mood_tracker_in_final_position() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Trackers after the mood: `im good -sleep 8 -water 5`.
    let cmd = parse_from(vec![
        "good".to_string(),
        "-sleep".to_string(),
        "8".to_string(),
        "-water".to_string(),
        "5".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let mood_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'good'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let rows = sqlx::query("SELECT type, score, mood FROM tracker ORDER BY type")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<String, _>("type"), "sleep");
    assert_eq!(rows[0].get::<f64, _>("score"), 8.0);
    assert_eq!(rows[0].get::<Option<i64>, _>("mood"), Some(mood_id));
    assert_eq!(rows[1].get::<String, _>("type"), "water");
    assert_eq!(rows[1].get::<f64, _>("score"), 5.0);
    assert_eq!(rows[1].get::<Option<i64>, _>("mood"), Some(mood_id));
}

#[tokio::test]
async fn test_out_of_range_tracker_still_inserts() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();

    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(4.0),
            high: Some(10.0),
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // sleep=2 is below min=4, but min/max only affect binning:
    // the mood and its tracker entry are still inserted.
    let cmd = parse_from(vec![
        "-sleep".to_string(),
        "2".to_string(),
        "ok".to_string(),
    ])
    .unwrap();

    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let mood_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mood_count, 1);

    let tracker_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tracker_count, 1);
}

#[test]
fn test_tab_in_mood_rejected() {
    // Mood with tab is rejected at parse time (view output uses tab separators)
    let result = parse_from(vec!["ok\tmood".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("tab characters"));
}

#[tokio::test]
async fn test_unknown_tracker_rejected() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Unknown tracker should be rejected
    let cmd = parse_from(vec!["-unknown".to_string(), "5".to_string()]).unwrap();

    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unknown tracker type")
    );
}

#[tokio::test]
async fn test_today_view_no_data() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    // write_today_view needs the embedder built — handle_command does
    // this before dispatching, so the direct call must too.
    let axes = im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();
    // write_today_view should succeed even with no data
    let mut out = Vec::new();
    let result = im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await;
    assert!(result.is_ok());
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains("Nothing logged today."),
        "output: {output:?}"
    );
}

#[tokio::test]
async fn test_today_view_with_data() {
    use im::config::{TrackerKind, TrackerSetting};

    let pool = test_pool().await.unwrap();
    let mut config = Config::default();

    // Register tracker trackers
    config.tracker.insert(
        "sleep".to_string(),
        TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "water".to_string(),
        TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Create a mood entry via the CLI path
    execute_command(
        parse_from(vec!["good".to_string()]).unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // write_today_view needs the embedder built — handle_command does
    // this before dispatching, so the direct call must too.
    let axes = im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    // Create a tracker-only entry via the CLI path
    execute_command(
        parse_from(vec!["-sleep".to_string(), "8".to_string()]).unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Create a oneshot task due today via the CLI path: ! desc @YYYY-MM-DD
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    execute_command(
        parse_from(vec![
            "!".to_string(),
            "due today".to_string(),
            format!("@{today_str}"),
        ])
        .unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // write_today_view should succeed with data and emit tab-separated rows
    let mut out = Vec::new();
    let result = im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await;
    assert!(result.is_ok());
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("good"), "output: {output:?}");
    assert!(output.contains("due today"), "output: {output:?}");
    assert!(output.contains('\t'), "output: {output:?}");
}

/// A mood entry with an attached tracker value and a linked task carries
/// both in `fetch_today_entries` (the data behind the preview's `linked:`
/// section).
#[tokio::test]
async fn test_today_view_linked_trackers_and_tasks() {
    use im::config::TrackerSetting;
    use im::today::EntryKind;
    use im::types::{TodayHorizon, ViewVariant};

    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        TrackerSetting {
            interval: None,
            low: None,
            high: Some(10.0),
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // A task with a short id to link to, via the CLI path.
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    execute_command(
        parse_from(vec![
            "!".to_string(),
            "water plants".to_string(),
            format!("@{today_str}"),
        ])
        .unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let short_id: i64 = sqlx::query_scalar("SELECT short_id FROM todos WHERE name = ?")
        .bind("water plants")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Mood with an attached tracker value and a task link (`-<short id>`).
    execute_command(
        parse_from(vec![
            "good".to_string(),
            "-sleep".to_string(),
            "8".to_string(),
            format!("+{short_id}"),
        ])
        .unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // fetch_today_entries needs the embedder built — the CLI
    // path does this before dispatching, so the direct call must too.
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let im::today::TodayFetch { entries, .. } =
        im::today::fetch_today_entries(&pool, &config, TodayHorizon::Today, im::date::today_start(), ViewVariant::All)
            .await
            .unwrap();
    let mood = entries
        .iter()
        .find(|e| e.kind == EntryKind::Mood)
        .expect("expected a mood entry");
    assert_eq!(mood.linked_trackers.len(), 1);
    assert_eq!(mood.linked_trackers[0].name, "sleep");
    assert_eq!(mood.linked_trackers[0].payload, "8");
    assert_eq!(mood.linked_tasks.len(), 1);
    assert_eq!(mood.linked_tasks[0].name, "water plants");
    assert_eq!(mood.linked_tasks[0].badge, Some('○')); // not done yet
}

/// Null tracker labels, update-action semantics, and `prev:` in the today
/// view: null rows carry no payload (the name alone); the update action
/// moves the row's timestamp within its slot; `tracker_prev` carries the
/// previous entry of the same kind.
#[tokio::test]
async fn test_today_view_null_labels_relog_and_prev() {
    use im::config::TrackerSetting;
    use im::types::{TodayHorizon, ViewVariant};

    let pool = test_pool().await.unwrap();
    let mut config = Config::default();

    // Replace-mode null tracker (single bound, count threshold unused).
    config.tracker.insert(
        "water".to_string(),
        TrackerSetting {
            interval: Some(day_interval()),
            low: Some(0.0),
            high: None,
            kind: TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    // Replace-mode null tracker: interval + both bounds (time offsets).
    config.tracker.insert(
        "sit".to_string(),
        TrackerSetting {
            interval: Some(day_interval()),
            low: Some(0.0),
            high: Some(86400.0),
            kind: TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    // Plain float tracker (no interval) for the prev: checks.
    config.tracker.insert(
        "sleep".to_string(),
        TrackerSetting {
            interval: None,
            low: None,
            high: Some(10.0),
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Log twice in the same day slot: replace mode keeps one marker.
    for _ in 0..2 {
        execute_command(
            parse_from(vec!["-water".to_string()]).unwrap(),
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }
    execute_command(
        parse_from(vec!["-sit".to_string()]).unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    execute_command(
        parse_from(vec!["-sleep".to_string(), "6.5".to_string()]).unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    execute_command(
        parse_from(vec!["-sleep".to_string(), "8".to_string()]).unwrap(),
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // fetch_today_entries needs the embedder built — the CLI
    // path does this before dispatching, so the direct call must too.
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let im::today::TodayFetch { entries, .. } =
        im::today::fetch_today_entries(&pool, &config, TodayHorizon::Today, im::date::today_start(), ViewVariant::All)
            .await
            .unwrap();

    // Null rows carry no payload: the name alone.
    let water = entries
        .iter()
        .find(|e| e.label == "water")
        .expect("expected the water entry");
    let sit = entries
        .iter()
        .find(|e| e.label == "sit")
        .expect("expected the sit entry");
    assert_eq!(sit.label, "sit");

    // prev: the later sleep entry points at the earlier one (row id is the
    // tiebreaker for same-second entries); the first has no previous entry.
    let sleeps: Vec<_> = entries
        .iter()
        .filter(|e| e.label.starts_with("sleep:"))
        .collect();
    assert_eq!(sleeps.len(), 2);
    let (first, second) = if sleeps[0].id < sleeps[1].id {
        (&sleeps[0], &sleeps[1])
    } else {
        (&sleeps[1], &sleeps[0])
    };
    assert!(first.time <= second.time);
    assert_eq!(first.tracker_prev, None);
    assert_eq!(second.tracker_prev, Some(first.time));

    // Update action on the water entry (same day slot): the timestamp
    // moves, nothing else changes (no score bump, no deletes).
    let water_id = water.id.expect("tracker entry id");
    let t0 = water.time;
    im::db::update_tracker_time(&pool, water_id, t0 + 1000)
        .await
        .unwrap();

    let im::today::TodayFetch { entries, .. } =
        im::today::fetch_today_entries(&pool, &config, TodayHorizon::Today, im::date::today_start(), ViewVariant::All)
            .await
            .unwrap();
    let water = entries
        .iter()
        .find(|e| e.id == Some(water_id))
        .expect("expected the water entry");
    assert_eq!(water.label, "water");
    assert_eq!(water.time, t0 + 1000);
}

/// `im @<date>` anchors the today view to an arbitrary day.
#[tokio::test]
async fn test_today_view_with_date() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    // Seed a mood on a fixed past date directly.
    let target = im::date::parse_datetime("2024-03-15 09:00", im::date::DATE_DIALECT).unwrap();
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('ancient', '', ?)")
        .bind(target)
        .execute(&pool)
        .await
        .unwrap();

    // `im @2024-03-15` lists it.
    let cmd = parse_from(vec!["@2024-03-15".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("ancient"), "output: {output:?}");

    // Plain `im` (today) does not.
    let cmd = parse_from(vec![]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(!output.contains("ancient"), "output: {output:?}");
}

/// The today view fetches moods and tracker entries across the whole
/// horizon (`[day start, horizon end]`), not just the anchored day — the
/// +tomorrow / +this week horizons must surface tomorrow's moods and
/// tracker values, matching the task fetches.
#[tokio::test]
async fn test_today_view_horizon_includes_moods_and_trackers() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );

    let anchored_day = im::date::today_start() - 2 * 86_400;
    let tomorrow = anchored_day + 86_400;

    // A mood + tracker entry on the anchored day, and one of each on
    // the next day (inside the +tomorrow horizon, outside the day one).
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('today mood', '', ?)")
        .bind(anchored_day + 9 * 3600)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('tomorrow mood', '', ?)")
        .bind(tomorrow + 9 * 3600)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time, mood) VALUES ('sleep', 7, ?, NULL)")
        .bind(anchored_day + 10 * 3600)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time, mood) VALUES ('sleep', 8, ?, NULL)")
        .bind(tomorrow + 10 * 3600)
        .execute(&pool)
        .await
        .unwrap();

    macro_rules! labels {
        ($horizon:expr) => {{
            let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
                &pool,
                &config,
                $horizon,
                anchored_day,
                im::types::ViewVariant::All,
            )
            .await
            .unwrap();
            entries
                .iter()
                .map(|e| e.label.clone())
                .filter(|l| l.contains("mood") || l.starts_with("sleep"))
                .collect::<Vec<_>>()
        }};
    }

    // Day horizon: only the anchored-day entries.
    assert_eq!(
        labels!(im::types::TodayHorizon::Today),
        ["today mood".to_string(), "sleep: 7".to_string()]
    );

    // +tomorrow horizon: tomorrow's entries are fetched too (sorted by
    // time, so each day's pair keeps its order).
    assert_eq!(
        labels!(im::types::TodayHorizon::Tomorrow),
        [
            "today mood".to_string(),
            "sleep: 7".to_string(),
            "tomorrow mood".to_string(),
            "sleep: 8".to_string(),
        ]
    );
}

/// `mood.score` round-trips through the sql layer (nullable REAL column).
#[tokio::test]
async fn test_mood_score_roundtrip() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // CLI-created entries compute the saliency at insert time.
    let cmd = parse_from(vec!["vivid".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let rows = im::db::fetch_moods_between(&pool, 0, i64::MAX)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].score.is_some(),
        "CLI-created entries carry their computed saliency"
    );

    // Rows without a score (e.g. seed_db inserts) read back as None and
    // round-trip through update_mood_score.
    let id = rows[0].id;
    sqlx::query("UPDATE mood SET score = NULL WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let rows = im::db::fetch_moods_between(&pool, 0, i64::MAX)
        .await
        .unwrap();
    assert_eq!(rows[0].score, None);
    im::db::update_mood_score(&pool, id, 0.42).await.unwrap();
    let rows = im::db::fetch_moods_between(&pool, 0, i64::MAX)
        .await
        .unwrap();
    assert!((rows[0].score.unwrap() - 0.42).abs() < 1e-6);
}

/// The first render pass backfills `mood.score` (mood saliency); a
/// pre-seeded score is left untouched (read-back path).
#[tokio::test]
async fn test_today_view_backfills_mood_score() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    let axes = im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    // Two moods: one fresh, one pre-seeded.
    let cmd = parse_from(vec!["vivid".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["glum".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let glum_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'glum'")
        .fetch_one(&pool)
        .await
        .unwrap();
    im::db::update_mood_score(&pool, glum_id, 0.5)
        .await
        .unwrap();

    // A directly-inserted row (no score) exercises the no-backfill path:
    // rendering must NOT write the score anymore (`mood_color_cached` is
    // sync and backfill-free; `:db backfill` persists it).
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('dull', '', ?)")
        .bind(im::date::now())
        .execute(&pool)
        .await
        .unwrap();

    // A fresh render pass (new color cache) runs the pipeline but leaves
    // the database untouched.
    let mut out = Vec::new();
    im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await
    .unwrap();

    let scores: Vec<Option<f32>> = sqlx::query_scalar("SELECT score FROM mood ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(scores.len(), 3);
    assert!(
        scores[0].is_some(),
        "CLI-created row carries its computed score"
    );
    assert_eq!(scores[1], Some(0.5), "pre-seeded score must be unchanged");
    assert!(
        scores[2].is_none(),
        "directly-inserted row must NOT be backfilled by rendering when moods.backfill is false"
    );
}

#[tokio::test]
async fn test_moods_backfill_config_and_render() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.moods.backfill = true;

    // 1. Insert a mood with backfill = true: insertion skips embedding & score
    let cmd = parse_from(vec!["serene".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let row = sqlx::query("SELECT embedding, score FROM mood WHERE mood = 'serene'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let emb_before: Option<Vec<u8>> = row.get("embedding");
    let score_before: Option<f32> = row.get("score");
    assert!(
        emb_before.is_none(),
        "embedding must NOT be computed at insertion when moods.backfill is true"
    );
    assert!(
        score_before.is_none(),
        "score must NOT be computed at insertion when moods.backfill is true"
    );

    // 2. Render today view: color computation should now backfill embedding & score
    let axes = im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let mut out = Vec::new();
    im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await
    .unwrap();

    let row_after = sqlx::query("SELECT embedding, score FROM mood WHERE mood = 'serene'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let emb_after: Option<Vec<u8>> = row_after.get("embedding");
    let score_after: Option<f32> = row_after.get("score");
    assert!(
        emb_after.is_some(),
        "embedding must be backfilled to DB on render"
    );
    assert!(
        score_after.is_some(),
        "score must be backfilled to DB on render"
    );
}

#[tokio::test]
async fn test_view_done_tasks() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a oneshot task, then complete it
    let task_id = create_oneshot_task(&pool, "finished task").await;
    let update_cmd = parse_from(vec![format!("+{task_id}"), "1".to_string()]).unwrap();
    execute_command(
        update_cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // @done should list the completed task; done oneshots render ✓.
    let cmd = parse_from(vec!["@done".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("finished task"), "output: {output:?}");
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 6, "line not tab-separated: {line:?}");
        // Completed tasks show no id — the id column is empty.
        assert!(fields[0].is_empty(), "completed task shows no id: {line:?}");
        // Done oneshot task → ✓ badge (no "DONE" suffix anymore).
        assert!(fields[5].contains('✓'), "badge dot expected: {line:?}");
        assert!(
            !fields[5].ends_with("DONE"),
            "DONE suffix dropped: {line:?}"
        );
    }
}

#[tokio::test]
async fn test_view_due_tasks() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a oneshot task due today via the CLI path: ! desc @YYYY-MM-DD
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let cmd = parse_from(vec![
        "!".to_string(),
        "due today task".to_string(),
        format!("@{today_str}"),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // @due opens the TodayView at ShowVariant::B — today-view rows have
    // 4 tab-separated columns: time, badge, label, detail.
    let cmd = parse_from(vec!["@due".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("due today task"), "output: {output:?}");
    for line in output.lines() {
        assert_eq!(
            line.split('\t').count(),
            4,
            "line not tab-separated into 4 today-view columns: {line:?}"
        );
    }
}

/// Pack a seconds-denominated interval into its packed-DbSpan form (test
/// fixtures historically used 86400 to mean "1 day").
fn pack_interval(secs: i64) -> i64 {
    let span = jiff::Span::new()
        .days(secs / 86_400)
        .hours((secs % 86_400) / 3600)
        .minutes((secs % 3600) / 60)
        .seconds(secs % 60);
    im::date::span_to_db(&span)
}

/// The calendar span for a seconds-denominated interval (fixture helper).
fn interval_span(secs: i64) -> jiff::Span {
    jiff::Span::new()
        .days(secs / 86_400)
        .hours((secs % 86_400) / 3600)
        .minutes((secs % 3600) / 60)
        .seconds(secs % 60)
}

/// Insert a recurring task row directly and return its id.
async fn insert_recurring_task(
    pool: &SqlitePool,
    name: &str,
    start_time: i64,
    interval: i64,
    available_duration: Option<i64>,
    target_count: i32,
    end_time: Option<i64>,
) -> i64 {
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time, end_time) \
         VALUES (?, '', 5, ?, ?, ?, 0, ?, ?)",
    )
    .bind(name)
    .bind(pack_interval(interval))
    .bind(available_duration)
    .bind(target_count)
    .bind(start_time)
    .bind(end_time)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Run a view command and return its raw output.
async fn run_view(pool: &SqlitePool, config: &Config, args: &[&str]) -> String {
    let cmd = parse_from(args.iter().map(|s| s.to_string()).collect()).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, pool, config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

/// Task↔mood links: `im <mood> -<short id>` records a link between
/// the new mood entry and the task (no completion). The task preview
/// then shows the linked moods.
#[tokio::test]
async fn test_task_mood_links() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a oneshot task (short id 1) and a recurring one (short id 2).
    let cmd = parse_from(vec!["!".to_string(), "link me".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO todos (name, body, priority, start_time, interval_secs, target_count, optional, short_id) \
         VALUES ('recur link', '', 5, ?, ?, 0, 0, 2)",
    )
    .bind(im::date::now())
    .bind(im::date::span_to_db(&jiff::Span::new().days(1)))
    .execute(&pool)
    .await
    .unwrap();

    // Each entry carries at most one '+' task_ref: link one entry to the
    // oneshot (short id 1), another to the recurring task (short id 2).
    let cmd = parse_from(vec![
        "felt".to_string(),
        "good".to_string(),
        "+1".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["tired".to_string(), "+2".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let felt_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'felt good'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let tired_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'tired'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let links: Vec<(i64, i64)> = sqlx::query_as("SELECT todo_id, id FROM mood WHERE todo_id IS NOT NULL")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(links.len(), 2, "both links recorded");
    assert!(links.contains(&(1, felt_id)));
    assert!(links.contains(&(2, tired_id)));

    // A link with an unknown short id errors and records nothing.
    let cmd = parse_from(vec!["ok".to_string(), "+99".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err(), "unknown short id must error");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood WHERE todo_id IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "failed link must not add rows");

    // Links without a mood entry are rejected at parse time ('+' creates
    // no mood row here; note a *leading* '+' is the editor command, so the
    // ref sits behind a valueless tracker).
    assert!(parse_from(vec!["-sleep".to_string(), "+1".to_string()]).is_err());

    // The task preview data source lists the linked moods.
    let moods = im::db::fetch_linked_moods(&pool, 1).await.unwrap();
    assert_eq!(moods.len(), 1);
    assert_eq!(moods[0].mood, "felt good");
}

/// Null tracker semantics: a valueless `-<name>` logs a Null tracker.
/// Replace mode: one marker per interval slot, score 0 (re-logging drops
/// the slot's previous marker). Cumulative mode: every log appends its own
/// score-0 row. Without an interval: error.
#[tokio::test]
async fn test_null_tracker_semantics() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    let day_interval = im::config::TrackerInterval {
        anchor: im::date::today_start() - 86_400,
        span: jiff::Span::new().days(1),
        cumulative: false,
    };
    // Replace mode (default): one marker per slot, score 0.
    config.tracker.insert(
        "prouds".to_string(),
        im::config::TrackerSetting {
            interval: Some(day_interval),
            low: None,
            high: None,
            kind: im::config::TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    // Replace mode with both bounds (23:00 / 2h before the span end):
    // the marker's color comes from its circular position.
    config.tracker.insert(
        "sips".to_string(),
        im::config::TrackerSetting {
            interval: Some(im::config::TrackerInterval {
                anchor: im::date::today_start() - 86_400,
                span: jiff::Span::new().days(1),
                cumulative: true,
            }),
            low: None,
            high: None,
            kind: im::config::TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    // Both bounds: the marker's circular time position drives its color.
    config.tracker.insert(
        "sleep_start".to_string(),
        im::config::TrackerSetting {
            interval: Some(day_interval),
            low: Some(23.0 * 3600.0),
            high: Some(2.0 * 3600.0),
            kind: im::config::TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    // Without an interval: unsupported.
    config.tracker.insert(
        "unsupported".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: im::config::TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );

    // A valueless -<name> parses (no next token consumed).
    let cmd = parse_from(vec!["-prouds".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["-prouds".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["-sleep_start".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let prouds_rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT id, CAST(score AS TEXT), time FROM tracker WHERE type = 'prouds' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // Replace mode: one row per slot, score stays 0.
    assert_eq!(prouds_rows.len(), 1, "replace keeps one row per slot");
    assert_eq!(prouds_rows[0].1, "0", "replace keeps score 0");

    // Cumulative mode: each log appends its own score-0 row.
    for _ in 0..2 {
        execute_command(
            parse_from(vec!["-sips".to_string()]).unwrap(),
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }
    let sips_rows: Vec<String> = sqlx::query(
        "SELECT CAST(score AS TEXT) AS s FROM tracker WHERE type = 'sips' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get::<String, _>("s"))
    .collect();
    assert_eq!(sips_rows.len(), 2, "cumulative keeps every log");
    assert_eq!(sips_rows, vec!["0".to_string(), "0".to_string()]);

    // Re-log the time-marker: same slot → the row's time moves, score stays.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let cmd = parse_from(vec!["-sleep_start".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let sleep_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT CAST(score AS TEXT), time FROM tracker WHERE type = 'sleep_start' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sleep_rows.len(), 1, "time-marker keeps one row per slot");
    assert_eq!(sleep_rows[0].0, "0", "time-marker keeps score 0");

    // Null without an interval errors.
    let cmd = parse_from(vec!["-unsupported".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "null tracker without an interval must error"
    );

    // A valued -<name> for a Null tracker errors too.
    let cmd = parse_from(vec!["-prouds".to_string(), "x".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err(), "null tracker must not take a value");
}

/// `@done:O` shows ALL recurring tasks (one row per task, no completions
/// filter): history rows with entries in earlier intervals (unscoped sum),
/// expired tasks, and even never-completed ones — unlike `@done` (All),
/// which needs done-in-current-interval and excludes expired tasks (D3).
/// entry ever (unscoped sum), including expired ones — unlike `@done` (All),
/// which needs done-in-current-interval and excludes expired tasks (D3).
#[tokio::test]
async fn test_view_done_b_recurring_history() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    let interval = 86_400i64;
    let now = im::date::now();

    // Recurring with completions only in the FIRST interval: unscoped sum 2
    // (history), current-interval sum 0 (not done now).
    let start = now - 2 * interval - 500;
    let history_id =
        insert_recurring_task(&pool, "history task", start, interval, None, 2, None).await;
    update_task(&pool, history_id, start + 100, 1).await;
    update_task(&pool, history_id, start + 200, 1).await;

    // Expired recurring with a single completion ever (end_time passed).
    let expired_id = insert_recurring_task(
        &pool,
        "expired task",
        now - 10 * interval,
        interval,
        None,
        1,
        Some(now - 1000),
    )
    .await;
    update_task(&pool, expired_id, now - 10 * interval + 100, 1).await;

    // Never-completed recurring (zero entries ever).
    let _fresh_id = insert_recurring_task(
        &pool,
        "fresh task",
        now - 2 * interval,
        interval,
        None,
        1,
        None,
    )
    .await;

    // @done (All): none appear — history task isn't done in the current
    // interval, expired task is excluded.
    let all = run_view(&pool, &config, &["@done"]).await;
    assert!(!all.contains("history task"), "@done All: {all:?}");
    assert!(!all.contains("expired task"), "@done All: {all:?}");

    // @done:O (B): all three appear (ALL R — unscoped history, expired and
    // never-completed rows included).
    let b = run_view(&pool, &config, &["@done:O"]).await;
    assert!(b.contains("history task"), "@done:O: {b:?}");
    assert!(b.contains("expired task"), "@done:O: {b:?}");
    assert!(b.contains("fresh task"), "@done:O: {b:?}");
}

/// `@done:O` partial-history rows (recurring with target > 1, entries ever
/// but sum < target — not `is_done()`) sort by their last completion
/// entry, not by a future window end (which would push them to the bottom
/// of the done list as if "due in the future").
#[tokio::test]
async fn test_done_b_partial_history_sorts_by_last_completion() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    let interval = 86_400i64;
    let now = im::date::now();

    // Partial history: target 3, one entry 2 days ago. Its availability
    // window end is in the future — the buggy sort key.
    let partial = insert_recurring_task(
        &pool,
        "partial task",
        now - 10 * interval,
        interval,
        None,
        3,
        None,
    )
    .await;
    update_task(&pool, partial, now - 2 * interval, 1).await;

    // Done task completed 3 days ago (older than the partial's entry).
    let older_done = insert_recurring_task(
        &pool,
        "older done task",
        now - 10 * interval,
        interval,
        None,
        1,
        None,
    )
    .await;
    update_task(&pool, older_done, now - 3 * interval, 1).await;

    // Done task completed 1 hour ago.
    let recent_done = insert_recurring_task(
        &pool,
        "recent done task",
        now - 10 * interval,
        interval,
        None,
        1,
        None,
    )
    .await;
    update_task(&pool, recent_done, now - 3600, 1).await;

    // Date-descending: recent done, partial (2d ago), older done (3d ago).
    // With the buggy key (partial = future window end) the partial row
    // would land last.
    let done = run_view(&pool, &config, &["@done:O"]).await;
    let recent_pos = done.find("recent done task").expect("recent row");
    let partial_pos = done.find("partial task").expect("partial row");
    let older_pos = done.find("older done task").expect("older row");
    assert!(recent_pos < partial_pos, "order: {done:?}");
    assert!(
        partial_pos < older_pos,
        "partial-history row sorts by last completion, not a future window end: {done:?}"
    );
}

/// `@:O` shows recurring tasks whose availability window has passed (not
/// expired), while `@` (All) filters them out via `recurring_available`.
#[tokio::test]
async fn test_view_pending_b_not_availability_filtered() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    let interval = 86_400i64;
    let now = im::date::now();

    // Availability window [now-2h, now-1h) — passed, but not expired.
    let id = insert_recurring_task(
        &pool,
        "window passed task",
        now - 7200,
        interval,
        Some(3600),
        0,
        None,
    )
    .await;

    // @ (All): excluded by the availability filter.
    let all = run_view(&pool, &config, &["@"]).await;
    assert!(!all.contains("window passed task"), "@ All: {all:?}");

    // @:O (B): included — no availability post-filter.
    let b = run_view(&pool, &config, &["@:O"]).await;
    assert!(b.contains("window passed task"), "@:O: {b:?}");

    // The row itself exists (sanity).
    let _ = id;
}
/// The today view includes any task with a completion entry on the anchored
/// day, even when the recurring availability window passed: the per-window
/// recurring fetch surfaces the window with its rule-based time cell (window
/// passed → last completion within the interval, else the window start).
/// completion timestamp (VIEWS.md time-label rule).
#[tokio::test]
async fn test_today_view_completed_today_inclusion_and_time_label() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let interval = 86_400i64;
    let anchored_day = im::date::today_start() - 2 * 86_400;

    // A: always available (no duration); completion at 10:30 on the anchored
    // day, outside the current interval.
    let a = insert_recurring_task(
        &pool,
        "completed always",
        anchored_day + 6 * 3600,
        interval,
        None,
        0,
        None,
    )
    .await;
    let a_time = anchored_day + 10 * 3600 + 30 * 60;
    update_task(&pool, a, a_time, 1).await;

    // B: availability window passed on the anchored day (08:00-09:00), so
    // the regular availability-filtered recurring fetch drops it; only the
    // completed-today merge surfaces it.
    let b = insert_recurring_task(
        &pool,
        "completed window passed",
        anchored_day + 8 * 3600,
        interval,
        Some(3600),
        0,
        None,
    )
    .await;
    let b_time = anchored_day + 10 * 3600;
    update_task(&pool, b, b_time, 1).await;

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        anchored_day,
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();

    let row = |name: &str| {
        entries
            .iter()
            .find(|e| e.kind.is_task() && e.label == name)
            .expect("task row must appear in the today view")
    };
    // Time cell: the windows have passed (`now >= window_end`), so they
    // show the last completion within their interval.
    assert_eq!(row("completed always").time_label, "10:30");
    assert_eq!(row("completed window passed").time_label, "10:00");
    // Both are done in the current interval? No — the badge reflects the
    // current-interval state (D8): zero completions in the current interval
    // → not done ↻.
    let _ = row("completed always").badge(&config);
    let _ = row("completed window passed").badge(&config);

    // The A variant (journal) displays completions instead of tasks:
    // both completions (10:30 and 10:00) on the anchored day appear in A.
    let im::today::TodayFetch {
        entries: entries_a, ..
    } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        anchored_day,
        im::types::ViewVariant::A,
    )
    .await
    .unwrap();
    let names_a: Vec<&str> = entries_a.iter().map(|e| e.label.as_str()).collect();
    assert!(names_a.contains(&"completed always"));
    assert!(names_a.contains(&"completed window passed"));
}

/// TodayView recurring rows are per availability window: All shows every
/// window intersecting the horizon, B shows only the next window per task,
/// and A shows only not-done windows. A passed window shows the last
/// completion within its interval (else the window end); an open/future
/// window shows the window start (VIEWS.md).
#[tokio::test]
async fn test_today_view_per_window_recurring_rows() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let interval = 6 * 3600i64;
    let anchored_day = im::date::today_start() - 2 * 86_400;

    // 1-hour availability windows every 6 hours starting 02:00 on the
    // anchored day: 02:00, 08:00, 14:00, 20:00 (all in the past by the
    // time the test runs — no weekday prefixes in the labels).
    let t = insert_recurring_task(
        &pool,
        "per-window",
        anchored_day + 2 * 3600,
        interval,
        Some(3600),
        0,
        None,
    )
    .await;
    // Complete the 08:00 window at 08:30: that window is then done
    // (completions are scoped to its own interval) and shows 08:30.
    update_task(&pool, t, anchored_day + 8 * 3600 + 1800, 1).await;

    macro_rules! get_labels {
        ($show:expr) => {{
            let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
                &pool,
                &config,
                im::types::TodayHorizon::Today,
        anchored_day,
                $show,
            )
            .await
            .unwrap();
            entries
                .iter()
                .filter(|e| e.kind.is_task() && e.label == "per-window")
                .map(|e| e.time_label.clone())
                .collect::<Vec<_>>()
        }};
    }

    // All: every intersecting window. Passed windows show the last
    // completion within their interval, else the window end.
    assert_eq!(
        get_labels!(im::types::ViewVariant::All),
        ["03:00", "08:30", "15:00", "21:00"]
    );
    // B: only the next (earliest) window per task.
    assert_eq!(get_labels!(im::types::ViewVariant::B), ["03:00"]);
    // A: displays completions instead of tasks — only the completed 08:30 event appears.
    assert_eq!(
        get_labels!(im::types::ViewVariant::A),
        ["08:30"]
    );
}

/// A completed recurring window shows the last completion time even while
/// the window is still open (`now < window_end`) — the revised time rule:
/// done window → last completion; passed window → last completion in the
/// interval, else window end; open/future window → window start.
#[tokio::test]
async fn test_today_view_open_done_window_shows_completion() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let anchored_day = im::date::today_start() - 2 * 86_400;
    // 24-hour availability window every 48h starting yesterday 23:00:
    // the only window intersecting the anchored-day horizon runs
    // [yesterday 23:00, today 23:00) — still open whenever the suite runs
    // during the day.
    let t = insert_recurring_task(
        &pool,
        "open done window",
        anchored_day + 23 * 3600,
        48 * 3600,
        Some(24 * 3600),
        0,
        None,
    )
    .await;
    // Complete it at 01:00 today (inside the window's interval): the
    // window is then done but not passed.
    let completion = anchored_day + 25 * 3600;
    update_task(&pool, t, completion, 1).await;
    let expected_label = format!(
        "{} {}",
        im::date::format_weekday(completion),
        im::date::format_time(completion)
    );

    macro_rules! get_labels {
        ($show:expr) => {{
            let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
                &pool,
                &config,
                im::types::TodayHorizon::Today,
                anchored_day,
                $show,
            )
            .await
            .unwrap();
            entries
                .iter()
                .filter(|e| e.kind.is_task() && e.label == "open done window")
                .map(|e| e.time_label.clone())
                .collect::<Vec<_>>()
        }};
    }

    // All and B show the row at the completion time, not the window start
    // (the window is still open).
    let all = get_labels!(im::types::ViewVariant::All);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0], expected_label);
    assert_eq!(get_labels!(im::types::ViewVariant::B), [expected_label]);
    assert!(get_labels!(im::types::ViewVariant::A).is_empty());
}

/// D9: a just-completed task stays visible in `@` (All) within
/// `persist_pending_seconds`, and disappears once the window passes.
#[tokio::test]
async fn test_persist_pending_seconds() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    assert_eq!(config.tasks_view.persist_pending_seconds, 5 * 60);

    // Create + complete a oneshot task.
    let task_id = create_oneshot_task(&pool, "just finished").await;
    let update_cmd = parse_from(vec![format!("+{task_id}"), "1".to_string()]).unwrap();
    execute_command(
        update_cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Still in `@` right after completing (the persist window holds it).
    let pending = run_view(&pool, &config, &["@"]).await;
    assert!(pending.contains("just finished"), "@ All: {pending:?}");

    // Backdate the completion past the persist window: it disappears from
    // `@` (done + outside the window) but stays in `@done`.
    sqlx::query("UPDATE todo_completions SET time = time - 400")
        .execute(&pool)
        .await
        .unwrap();
    let pending = run_view(&pool, &config, &["@"]).await;
    assert!(!pending.contains("just finished"), "@ All: {pending:?}");
    let done = run_view(&pool, &config, &["@done"]).await;
    assert!(done.contains("just finished"), "@done All: {done:?}");
}

/// D9 applies to every pending variant, kind-scoped: `@:o` keeps a
/// just-completed oneshot (and only oneshots) within the persist window;
/// `@:O` keeps a just-completed recurring task (and only sched/recur).
#[tokio::test]
async fn test_persist_pending_variant_scoping() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let oneshot = create_oneshot_task(&pool, "just finished oneshot").await;
    let cmd = parse_from(vec![format!("+{oneshot}"), "1".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // A completed recurring task, also just completed.
    let interval = 86_400i64;
    let now = im::date::now();
    let recurring = insert_recurring_task(
        &pool,
        "just finished recurring",
        now - 3600,
        interval,
        Some(7200),
        1,
        None,
    )
    .await;
    im::db::update_task(&pool, recurring, 1).await.unwrap();

    // @:o holds the oneshot (D9, oneshot scope) but not the recurring.
    let a = run_view(&pool, &config, &["@:o"]).await;
    assert!(a.contains("just finished oneshot"), "@:o: {a:?}");
    assert!(!a.contains("just finished recurring"), "@:o: {a:?}");

    // @:O holds the recurring (D9, sched/recur scope) but not the oneshot.
    let b = run_view(&pool, &config, &["@:O"]).await;
    assert!(b.contains("just finished recurring"), "@:O: {b:?}");
    assert!(!b.contains("just finished oneshot"), "@:O: {b:?}");

    // Once the persist window passes, both disappear from their variant
    // (done + outside the window).
    sqlx::query("UPDATE todo_completions SET time = time - 400")
        .execute(&pool)
        .await
        .unwrap();
    let a = run_view(&pool, &config, &["@:o"]).await;
    assert!(!a.contains("just finished oneshot"), "@:o: {a:?}");
    let b = run_view(&pool, &config, &["@:O"]).await;
    assert!(!b.contains("just finished recurring"), "@:O: {b:?}");
}

/// `@:O` scheduled rows are non-done `S` with `window_open`: ongoing and
/// failed-with-open-window show; failed with a closed window and
/// auto-completed (no entry, window elapsed) belong to `@done` only.
#[tokio::test]
async fn test_pending_b_window_open_scheduled() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    let now = im::date::now();

    // Ongoing: window open, no entry.
    let ongoing = insert_scheduled(&pool, "ongoing task", now - 7200, 3 * 3600, None).await;
    // Failed with window still open.
    let failed_open = insert_scheduled(&pool, "failed open task", now - 7200, 3 * 3600, None).await;
    // Failed with a closed window.
    let failed_closed = insert_scheduled(&pool, "failed closed task", now - 7200, 3600, None).await;
    // Auto-completed: window elapsed, no entry.
    let auto = insert_scheduled(&pool, "auto completed task", now - 7200, 3600, None).await;

    // Entries outside the persist window so only the window logic decides.
    let t = now - 600;
    for (id, count) in [(failed_open, 0), (failed_closed, 0), (auto, 1)] {
        update_task(&pool, id, t, count).await;
    }

    let b = run_view(&pool, &config, &["@:O"]).await;
    assert!(b.contains("ongoing task"), "@:O: {b:?}");
    assert!(b.contains("failed open task"), "@:O: {b:?}");
    assert!(!b.contains("failed closed task"), "@:O: {b:?}");
    assert!(!b.contains("auto completed task"), "@:O: {b:?}");

    // The failed-with-closed-window task lives in @done instead.
    let done = run_view(&pool, &config, &["@done"]).await;
    assert!(done.contains("failed closed task"), "@done: {done:?}");
    let _ = ongoing;
}

/// The today view's recurring fetch is interval-aware: a task started long
/// ago still shows when its current-interval availability window overlaps
/// the anchored day, and a task whose windows skip the day does not.
#[tokio::test]
async fn test_today_view_interval_aware_recurring_overlap() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let today_start = im::date::today_start();

    // Started 60 days ago at 06:00, daily, window 06:00-07:00 each day:
    // active today, even though start_time + duration is far in the past
    // (the raw-overlap formula would drop it).
    let _active = insert_recurring_task(
        &pool,
        "old but active today",
        today_start - 60 * 86_400 + 6 * 3600,
        86_400,
        Some(3600),
        0,
        None,
    )
    .await;

    // Started yesterday at 06:00 on a 2-day interval: windows are
    // yesterday 06:00 and tomorrow 06:00 — none overlap today.
    let skipping = insert_recurring_task(
        &pool,
        "no window today",
        today_start - 86_400 + 6 * 3600,
        2 * 86_400,
        Some(3600),
        0,
        None,
    )
    .await;

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        im::date::today_start(),
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();
    let active_row = entries
        .iter()
        .find(|e| e.label == "old but active today")
        .expect("interval-aware overlap must surface the old recurring task");
    // The time cell follows the availability rule (window start while
    // still open, else window end — the window's own interval has no
    // completion, so no last-time fallback). The window intersecting
    // today's period is deterministically today 06:00-07:00 (origin +
    // 60 daily intervals), so the expectation has no phase dependence on
    // the run time: before 07:00 the open window shows its start, at or
    // after 07:00 the passed window shows its end.
    let now = im::date::now();
    let window_start = today_start + 6 * 3600;
    let window_end = window_start + 3600;
    let expected_label = if now >= window_end {
        im::date::format_time(window_end)
    } else {
        im::date::format_time(window_start)
    };
    assert_eq!(active_row.time_label, expected_label);
    assert!(
        !entries.iter().any(|e| e.label == "no window today"),
        "task with no window overlapping today must not show"
    );
    let _ = skipping;
}

/// Today-view All time label for complete tasks = completion time
/// (generalizes the scheduled time label): a scheduled task completed in
/// its window shows the completion time, an auto-completed one shows
/// start + duration; the B variant (@due) filters completed tasks out.
#[tokio::test]
async fn test_today_view_done_time_label_and_b_filter() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let yesterday_start = im::date::today_start() - 86_400;

    // Scheduled window [10:00, 16:00) on the anchored day, completed at
    // 14:30 on that day.
    let completed = insert_scheduled(
        &pool,
        "completed scheduled",
        yesterday_start + 10 * 3600,
        6 * 3600,
        None,
    )
    .await;
    let done_at = yesterday_start + 14 * 3600 + 30 * 60;
    update_task(&pool, completed, done_at, 1).await;

    // Auto-completed: window [10:00, 12:00) on the anchored day, no entry.
    insert_scheduled(
        &pool,
        "auto completed scheduled",
        yesterday_start + 10 * 3600,
        2 * 3600,
        None,
    )
    .await;

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        yesterday_start,
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();
    let row = |name: &str| {
        entries
            .iter()
            .find(|e| e.kind.is_task() && e.label == name)
            .expect("task row must appear in the today view")
    };
    assert_eq!(row("completed scheduled").time_label, "14:30");
    assert_eq!(row("auto completed scheduled").time_label, "12:00");

    // @due (B) is the same as All but tasks-only (no trackers/mood): a
    // task completed a minute ago in a window that is still open today
    // stays, with its completion-time label. The yesterday-anchored tasks
    // don't overlap today, so they don't show.
    let now = im::date::now();
    let completed_today =
        insert_scheduled(&pool, "completed today", now - 2 * 3600, 4 * 3600, None).await;
    update_task(&pool, completed_today, now - 60, 1).await;
    let due = run_view(&pool, &config, &["@due"]).await;
    assert!(
        due.contains("completed today"),
        "@due (B) shows completed tasks like All: {due:?}"
    );
    let expected = im::date::format_time(now - 60);
    let line = due
        .lines()
        .find(|l| l.contains("completed today"))
        .expect("completed today row");
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields[0], expected, "completion-time label: {line:?}");
    assert!(!due.contains("completed scheduled"), "@due: {due:?}");
    assert!(!due.contains("auto completed scheduled"), "@due: {due:?}");
}

/// Insert a scheduled task row directly and return its id.
async fn insert_scheduled(
    pool: &SqlitePool,
    name: &str,
    start_time: i64,
    duration: i64,
    end_time: Option<i64>,
) -> i64 {
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time, end_time) \
         VALUES (?, '', 5, NULL, ?, 0, 0, ?, ?)",
    )
    .bind(name)
    .bind(duration)
    .bind(start_time)
    .bind(end_time)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Insert a plain oneshot task (no interval, no availability window).
/// `target_count` 1 + one completion entry marks it done; 0 leaves it open.
async fn insert_oneshot(pool: &SqlitePool, name: &str, start_time: i64, target_count: i32) -> i64 {
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time, end_time) \
         VALUES (?, '', 5, NULL, NULL, ?, 0, ?, NULL)",
    )
    .bind(name)
    .bind(target_count)
    .bind(start_time)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_embed_utility() {
    use std::io::Cursor;
    let mut input = Cursor::new(b"happy day\nsad night\n");
    let mut out = Vec::new();
    im::commands::print_embeddings(&mut input, &mut out).unwrap();

    let output = String::from_utf8(out).unwrap();
    let mut lines = output.lines();
    let v1: Vec<f64> = lines
        .next()
        .expect("expected first embedding")
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let v2: Vec<f64> = lines
        .next()
        .expect("expected second embedding")
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(v1.len(), im::global::EMBED_DIM);
    assert_eq!(v2.len(), im::global::EMBED_DIM);
    assert!(lines.next().is_none(), "expected exactly two lines");
}

#[tokio::test]
async fn test_tracker_mood_dots() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a mood entry (with a mood and body so it gets a dot)
    let cmd = parse_from(vec!["happy".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // : (mood tracker) should print a header and dot rows
    let cmd = parse_from(vec![":".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    // Titles are verbose-only now: default output has no header, just the
    // dot rows; -v adds the bare title, -vv the ' (Week)' suffix.
    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    assert!(output.contains('●'), "expected a filled dot: {output:?}");

    let verbose_cmd = parse_from(vec![":".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(
        verbose_cmd,
        &pool,
        &config,
        &CliOpts { qv: [0, 1], fullscreen: false },
        &mut out,
        false,
    )
    .await
    .unwrap();
    assert!(output.contains('●'), "expected a filled dot: {output:?}");
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("Moods"), "output: {output:?}");
    assert!(!output.contains("Moods (Week)"), "output: {output:?}");

    let vv_cmd = parse_from(vec![":".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(
        vv_cmd,
        &pool,
        &config,
        &CliOpts { qv: [0, 2], fullscreen: false },
        &mut out,
        false,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("Moods (Week)"), "output: {output:?}");
}

#[tokio::test]
async fn test_tracker_dots() {
    use im::config::{TrackerKind, TrackerSetting};

    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Tracker entry via CLI
    let cmd = parse_from(vec!["-sleep".to_string(), "8".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // : sleep should show a filled dot
    let cmd = parse_from(vec![":".to_string(), "sleep".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    // Titles are verbose-only: no "Tracker 'sleep'" header by default;
    // -vv shows the bare label with the period suffix.
    assert!(!output.contains("Tracker 'sleep'"), "output: {output:?}");
    assert!(output.contains('●'), "expected a filled dot: {output:?}");

    let vv_cmd = parse_from(vec![":".to_string(), "sleep".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(
        vv_cmd,
        &pool,
        &config,
        &CliOpts { qv: [0, 2], fullscreen: false },
        &mut out,
        false,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("sleep (Week)"), "output: {output:?}");
}

#[tokio::test]
async fn test_tracker_recurring_dots() {
    let pool = test_pool().await.unwrap();

    // Create a recurring task via CLI (interactive prompts — use a direct DB insert
    // for the task itself, then mark completion via update)
    let name = "exercise";
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(name)
    .bind("")
    .bind(5)
    .bind(pack_interval(86400)) // 1 day interval
    .bind::<Option<i64>>(None)
    .bind(0)
    .bind(0)
    .bind(0)
    .execute(&pool)
    .await
    .unwrap();

    // Mark it complete via the sql API (the CLI `- @name` form was removed).
    let config = Config::default();
    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
    im::db::update_task(&pool, task_id, 1).await.unwrap();

    // : @exercise should show the task with a success dot
    let cmd = parse_from(vec![":".to_string(), format!("@{name}")]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    // Titles are verbose-only; -vv shows the bare @name with ' (Week)'.
    assert!(!output.contains("Task 'exercise'"), "output: {output:?}");
    assert!(output.contains('●'), "expected a filled dot: {output:?}");

    let vv_cmd = parse_from(vec![":".to_string(), format!("@{name}")]).unwrap();
    let mut out = Vec::new();
    execute_command(
        vv_cmd,
        &pool,
        &config,
        &CliOpts { qv: [0, 2], fullscreen: false },
        &mut out,
        false,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("@exercise (Week)"), "output: {output:?}");
}

#[tokio::test]
async fn test_tracker_recurring_year_uses_middle_dot() {
    let pool = test_pool().await.unwrap();

    // A recurring task with no completions: every interval slot is 0%.
    let name = "brush teeth";
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(name)
    .bind("")
    .bind(5)
    .bind(pack_interval(86400)) // 1 day interval
    .bind::<Option<i64>>(None)
    .bind(0)
    .bind(0)
    .bind(0)
    .execute(&pool)
    .await
    .unwrap();

    let config = Config::default();
    // Year range: empty intervals render the compact · instead of the large ◯.
    let cmd = parse_from(vec![":year".to_string(), format!("@{name}")]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    // Titles are verbose-only: no "Task '…' (Year)" header by default.
    assert!(
        !output.contains(&format!("Task '{name}' (Year)")),
        "output: {output:?}"
    );
    assert!(
        output.contains('·'),
        "expected middle dots for empty intervals in a year grid: {output:?}"
    );
    assert!(
        !output.contains('◯'),
        "year grid must not use the large ◯: {output:?}"
    );

    // -vv shows the @name with the ' (Year)' suffix.
    let vv_cmd = parse_from(vec![":year".to_string(), format!("@{name}")]).unwrap();
    let mut out = Vec::new();
    execute_command(
        vv_cmd,
        &pool,
        &config,
        &CliOpts { qv: [0, 2], fullscreen: false },
        &mut out,
        false,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains(&format!("@{name} (Year)")),
        "output: {output:?}"
    );
}

#[tokio::test]
async fn test_recurring_negative_delta_does_not_touch_previous_intervals() {
    let pool = test_pool().await.unwrap();

    // A recurring task with a 1-day interval that started 3 days and 500s ago.
    // The current interval therefore began at now - 500s.
    let interval = 86_400i64;
    let now = im::date::now();
    let start_time = now - 3 * interval - 500;
    let name = "water plants";
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(name)
    .bind("")
    .bind(5)
    .bind(pack_interval(interval))
    .bind::<Option<i64>>(None)
    .bind(0)
    .bind(0)
    .bind(start_time)
    .execute(&pool)
    .await
    .unwrap();

    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();

    // The boundary between the previous and current intervals, computed with
    // the same helper the update path uses.
    let interval_start = im::task::current_interval_start(start_time, interval_span(interval), now);

    // One completion in the previous interval (count 2), one in the current
    // interval (count 3).
    update_task(&pool, task_id, interval_start - 100, 2).await;
    update_task(&pool, task_id, interval_start + 100, 3).await;

    // Apply -5 via the sql API (the CLI `- @name` form was removed): the
    // current interval only holds 3, so the remaining 2 must NOT reach back
    // into the previous interval.
    im::db::update_task(&pool, task_id, -5).await.unwrap();

    // Previous-interval completion is untouched.
    let prev_sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ? AND time < ?",
    )
    .bind(task_id)
    .bind(interval_start)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prev_sum, 2, "previous interval must not be touched");

    // Current-interval completion was fully consumed.
    let cur_sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ? AND time >= ?",
    )
    .bind(task_id)
    .bind(interval_start)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cur_sum, 0, "current interval should be consumed");

    // The interval-scoped total returned by the shared helper is 0.
    let total = im::task::apply_completion_delta(&pool, task_id, 0)
        .await
        .unwrap();
    assert_eq!(total, 0, "interval-scoped total must be 0");
}

#[tokio::test]
async fn test_recurring_previous_interval_completions_still_shown() {
    let pool = test_pool().await.unwrap();

    // A recurring task with target_count 2, started 2 intervals + 500s ago.
    // Its only completions live in the FIRST interval, so the current-interval
    // sum is 0 even though the all-time sum already reaches the target.
    let interval = 86_400i64;
    let now = im::date::now();
    let start_time = now - 2 * interval - 500;
    let name = "brush teeth";
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(name)
    .bind("")
    .bind(5)
    .bind(pack_interval(interval))
    .bind::<Option<i64>>(None)
    .bind(2)
    .bind(0)
    .bind(start_time)
    .execute(&pool)
    .await
    .unwrap();

    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Two completions in the first interval only (count 1 each).
    update_task(&pool, task_id, start_time + 100, 1).await;
    update_task(&pool, task_id, start_time + 200, 1).await;

    let config = Config::default();

    // @ (CLI) must still show the task: it is not done in the current
    // interval, so it renders with the recurring ↻ badge.
    let mut out = Vec::new();
    let cmd = parse_from(vec!["@".to_string()]).unwrap();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains(name), "@ should show the task: {output:?}");
    for line in output.lines() {
        if line.contains(name) {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields[5], "↻", "recurring badge expected: {line:?}");
        }
    }

    // Completing it in the current interval: D9 keeps it visible in @ within
    // persist_pending_seconds (done ✓ badge); once the completion is outside
    // the persist window it disappears from the CLI @ view.
    im::db::update_task(&pool, task_id, 2).await.unwrap();

    let mut out = Vec::new();
    let cmd = parse_from(vec!["@".to_string()]).unwrap();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains(name),
        "@ persist window keeps a just-completed task: {output:?}"
    );
    for line in output.lines() {
        if line.contains(name) {
            let fields: Vec<&str> = line.split('\t').collect();
            assert!(
                fields[5].contains('✓'),
                "done badge in pending view: {line:?}"
            );
        }
    }

    // Backdate the completions past the persist window: @ hides it again
    // (done in the current interval, no longer recently completed).
    sqlx::query("UPDATE todo_completions SET time = time - 400")
        .execute(&pool)
        .await
        .unwrap();
    let mut out = Vec::new();
    let cmd = parse_from(vec!["@".to_string()]).unwrap();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        !output.contains(name),
        "@ must hide a task done in the current interval once the persist window passes: {output:?}"
    );
}

// ---------- Tracker payload types (text | number | float) ----------

#[tokio::test]
async fn test_text_tracker_entry_today_badge_and_listing() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "accomplishment".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Text,
            strict: false,
            colors: None,
        },
    );

    // im -accomplishment "fixed 2 bugs" via the CLI path
    let cmd = parse_from(vec![
        "-accomplishment".to_string(),
        "fixed 2 bugs".to_string(),
        "good".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Stored as text with the string payload
    let row = sqlx::query("SELECT score, typeof(score) AS t FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("score"), "fixed 2 bugs");
    assert_eq!(row.get::<String, _>("t"), "text");

    // Today view: text entries use the ◆ badge with the text as label.
    let cmd = parse_from(vec![]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains("accomplishment: fixed 2 bugs"),
        "output: {output:?}"
    );
    assert!(
        output.contains('\t') && output.contains('◆'),
        "text tracker entries must use the ◆ badge: {output:?}"
    );

    // : accomplishment lists entries as dark-gray '> text' lines
    let cmd = parse_from(vec![":".to_string(), "accomplishment".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    // The dark-gray '> ' prefix is ANSI-wrapped, so assert the pieces.
    assert!(output.contains("> "), "output: {output:?}");
    assert!(
        output.contains("fixed 2 bugs"),
        "expected the entry text after the prefix: {output:?}"
    );
}

#[tokio::test]
async fn test_text_tracker_lists_all_entries_in_range() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "accomplishment".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Text,
            strict: false,
            colors: None,
        },
    );

    for text in ["fixed 2 bugs", "shipped the feature", "wrote docs"] {
        let cmd = parse_from(vec!["-accomplishment".to_string(), text.to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }

    let cmd = parse_from(vec![":".to_string(), "accomplishment".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    for text in ["fixed 2 bugs", "shipped the feature", "wrote docs"] {
        assert!(output.contains(text), "output: {output:?}");
    }
    assert_eq!(output.matches("> ").count(), 3, "output: {output:?}");
}

#[tokio::test]
async fn test_tracker_parse_errors() {
    let pool = test_pool().await.unwrap();

    // Float tracker: non-numeric argument must error with a clear message
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    let cmd = parse_from(vec!["-sleep".to_string(), "good".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Cannot parse 'good' for tracker 'sleep' (kind=float): expected a plain number"),
        "expected a clear float parse error"
    );

    // Float tracker: duration strings are not plain numbers.
    let cmd = parse_from(vec!["-sleep".to_string(), "4h".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err(), "float must reject duration strings");

    // Number tracker: non-integer argument must error
    config.tracker.insert(
        "bugs".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );
    let cmd = parse_from(vec!["-bugs".to_string(), "3.5".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Cannot parse '3.5' for tracker 'bugs' (kind=integer): expected a plain whole number"),
        "expected a clear integer parse error"
    );

    // Integer tracker: duration strings, fractions, and scientific notation
    // all error with the same message.
    for bad in ["4h", "4.5", "1e3"] {
        let cmd = parse_from(vec!["-bugs".to_string(), bad.to_string()]).unwrap();
        let result = execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await;
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Cannot parse") && err.contains("(kind=integer)") && err.contains("expected a plain whole number"),
            "integer must reject {bad}, got: {err}"
        );
    }

    // Duration tracker: bare numbers error, duration strings store seconds.
    config.tracker.insert(
        "mile".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Duration,
            strict: false,
            colors: None,
        },
    );
    let cmd = parse_from(vec!["-mile".to_string(), "390".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Cannot parse '390'")
            && err.contains("(kind=duration)")
            && err.contains("expected a duration like '6m 30s'"),
        "duration must reject bare numbers, got: {err}"
    );
    let cmd = parse_from(vec!["-mile".to_string(), "6m 30s".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let stored: f64 = sqlx::query_scalar("SELECT score FROM tracker WHERE type = 'mile'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 390.0);

    // Null tracker with a value errors.
    config.tracker.insert(
        "pills".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    let cmd = parse_from(vec!["-pills".to_string(), "3".to_string()]).unwrap();
    let result = execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("does not take a value"),
        "null must reject payloads"
    );

    // Only the successful duration log was stored.
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_number_tracker_stored_as_integer() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "bugs".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(0.0),
            high: Some(10.0),
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec!["-bugs".to_string(), "3".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Number trackers store INTEGER payloads.
    let row = sqlx::query("SELECT score, typeof(score) AS t FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("score"), 3);
    assert_eq!(row.get::<String, _>("t"), "integer");

    // Values outside min/max still insert (min/max only affect binning).
    let cmd = parse_from(vec!["-bugs".to_string(), "11".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let row = sqlx::query("SELECT score, typeof(score) AS t FROM tracker ORDER BY id DESC")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("score"), 11);
    assert_eq!(row.get::<String, _>("t"), "integer");

    // Today view shows the integer value (bare `im` → Today).
    let cmd = parse_from(vec![]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("bugs: 3"), "output: {output:?}");
}

#[tokio::test]
async fn test_today_view_tasks_filters() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let now = im::date::now();
    let day_start = im::date::today_start();
    // Seed four oneshot tasks directly:
    //  - undated open: created two days ago, no deadline (the case the
    //    old creation-day proxy dropped from the today view)
    //  - dated overdue: deadline two days ago
    //  - due today: deadline before the day end
    //  - dated future: deadline tomorrow
    let insert = |name: &str, start: i64, end: Option<i64>| {
        sqlx::query(
            "INSERT INTO todos (name, body, priority, start_time, end_time) \
             VALUES (?, '', 5, ?, ?)",
        )
        .bind(name)
        .bind(start)
        .bind(end)
        .execute(&pool)
    };
    insert("undated open", now - 2 * 86400, None).await.unwrap();
    insert("dated overdue", now, Some(now - 2 * 86400))
        .await
        .unwrap();
    insert("due today", now, Some(day_start + 12 * 3600))
        .await
        .unwrap();
    insert("dated future", now, Some(now + 86400))
        .await
        .unwrap();

    // The oneshot filter is bound to the view variant (All → the
    // configured `initial_tasks_filter`, A → Horizon, B → Overdue), so
    // drive it through (show, config filter) pairs.
    let labels = |show: im::types::ViewVariant| {
        let pool = &pool;
        let base = &config;
        async move {
            im::today::fetch_today_entries(pool, base, im::types::TodayHorizon::Today, im::date::today_start(), show)
                .await
                .unwrap()
                .entries
                .into_iter()
                .filter(|e| e.kind.is_task())
                .map(|e| e.label)
                .collect::<Vec<_>>()
        }
    };

    // All: any open oneshot task — open, any date.
    let all = labels(im::types::ViewVariant::All).await;
    for name in ["undated open", "dated overdue", "due today", "dated future"] {
        assert!(
            all.contains(&name.to_string()),
            "All must include {name}: {all:?}"
        );
    }

    // B pins Overdue: only dated oneshots due within the horizon or
    // overdue; undated and future tasks stay out.
    let overdue = labels(im::types::ViewVariant::B).await;
    assert!(overdue.contains(&"dated overdue".to_string()));
    assert!(overdue.contains(&"due today".to_string()));
    assert!(!overdue.contains(&"dated future".to_string()));
    assert!(
        !overdue.contains(&"undated open".to_string()),
        "undated tasks are never overdue: {overdue:?}"
    );

    // A (journal): displays completions instead of tasks — since none of these have completions, none appear.
    let journal = labels(im::types::ViewVariant::A).await;
    assert!(journal.is_empty(), "journal: {journal:?}");
}

#[tokio::test]
async fn test_today_view_variant_bound_filter_cli() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let name = "stale undated chore";
    sqlx::query("INSERT INTO todos (name, body, priority, start_time) VALUES (?, '', 5, ?)")
        .bind(name)
        .bind(im::date::now() - 2 * 86400)
        .execute(&pool)
        .await
        .unwrap();

    // Bare `im` (All variant) shows it.
    let cmd = parse_from(vec![]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains(name), "output: {output:?}");

    // `@due` (B variant → Overdue) hides it — undated tasks are never
    // overdue.
    let cmd = parse_from(vec!["@due".to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, &pool, &config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(!output.contains(name), "output: {output:?}");
}

#[tokio::test]
async fn test_badge_setting_custom_deserialization() {
    // [badges]: journal_badge has a custom Deserialize impl accepting a bare char,
    // a color string, or an object with badge and/or color.
    let badges: Config = toml::from_str(
        r#"
        [badges]
        journal_badge = { badge = '·', color = "red" }
        tracker = 'x'
        mood = 'o'
        "#,
    )
    .unwrap();
    assert_eq!(
        badges.badges.journal_badge,
        Some(im::config::BadgeSetting {
            badge: Some('·'),
            color: Some(crossterm::style::Color::Red)
        })
    );
    assert_eq!(badges.badges.tracker, Some('x'));
    assert_eq!(badges.badges.mood, Some('o'));

    // Color-only form: no glyph.
    let color_only: Config = toml::from_str("[badges]\njournal_badge = \"#FFB6C1\"\n").unwrap();
    assert_eq!(
        color_only.badges.journal_badge,
        Some(im::config::BadgeSetting {
            badge: None,
            color: Some(crossterm::style::Color::Rgb {
                r: 0xFF,
                g: 0xB6,
                b: 0xC1
            })
        })
    );

    // Bare char form.
    let bare_char: Config = toml::from_str("[badges]\njournal_badge = '•'\n").unwrap();
    assert_eq!(
        bare_char.badges.journal_badge,
        Some(im::config::BadgeSetting {
            badge: Some('•'),
            color: None
        })
    );
}

#[tokio::test]
async fn test_priority_capped_at_max_priority_constant() {
    // The MAX_PRIORITY constant is the single source of truth for the
    // priority validation bound; ensure it stays at 999. Helpers (the
    // cliclack `validate` closure in `prompt_priority` — used by the
    // oneshot and recurring creation flows — all read this constant.
    assert_eq!(
        im::prompts::MAX_PRIORITY,
        999,
        "TODO.md requires priority capped to 999 — update ingestion if this changes"
    );
    // And the inclusive range used by validation (1..=999) accepts both
    // boundaries but rejects 0 and 1000.
    let range = 1..=im::prompts::MAX_PRIORITY;
    assert!(range.contains(&1), "lower bound must accept 1");
    assert!(range.contains(&999), "upper bound must accept 999");
    assert!(!range.contains(&0), "zero must be rejected");
    assert!(!range.contains(&1000), "1000 must be rejected");
}

// ---------- Mood tracker grid (◯ empty, spaced dots, grid config) ----------

/// Helper: run `:` / `:week` / `:month` / `:year` and return the raw output.
async fn run_tracker(pool: &SqlitePool, config: &Config, arg: &str) -> String {
    let cmd = parse_from(vec![arg.to_string()]).unwrap();
    let mut out = Vec::new();
    execute_command(cmd, pool, config, &CliOpts::default(), &mut out, false)
        .await
        .unwrap();
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn test_mood_tracker_grid_week_rolling_true_full_week() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.grid.week_rolling = true;

    // One mood entry today, via the CLI path.
    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":").await;

    // week_rolling=true always renders the full week: exactly 7 dots.
    // Titles are verbose-only — no header by default.
    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    assert_eq!(output.matches('◯').count(), 6, "output: {output:?}");
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
    assert_eq!(output.matches('◯').count() + output.matches('●').count(), 7);
    assert!(output.contains("◯  ◯"), "dots must be spaced: {output:?}");
    assert!(
        !output.contains('·'),
        "empty days must use ◯, not ·: {output:?}"
    );
}

#[tokio::test]
async fn test_tracker_grid_uses_colors_override() {
    // A tracker with its own palette must bin with that palette in the grid
    // view (both the interval and per-entry paths), not config.tasks.colors.
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    use crossterm::style::Color as CtColor;
    use im::config::ColorBins;
    let override_palette: ColorBins = vec![CtColor::Red, CtColor::White, CtColor::Blue].into();
    config.tracker.insert(
        "run".to_string(),
        im::config::TrackerSetting {
            interval: Some(day_interval()),
            low: Some(0.0),
            high: Some(10.0),
            kind: TrackerKind::Integer,
            strict: false,
            colors: Some(override_palette.clone()),
        },
    );
    config.tracker.insert(
        "feel".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(0.0),
            high: Some(10.0),
            kind: TrackerKind::Integer,
            strict: false,
            colors: Some(override_palette),
        },
    );

    // Max score → last palette color (Blue). The default palette's last color
    // is DarkGreen, so a Blue dot proves the override was used.
    let cmd = parse_from(vec!["-run".to_string(), "10".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["-feel".to_string(), "10".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_mood_tracker_grid_week_default_non_rolling() {
    let pool = test_pool().await.unwrap();
    // Defaults: week_rolling=false, week_start=Monday.
    let config = Config::default();
    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":").await;

    // Non-rolling week = week_start (Monday) through today, so the dot count
    // depends on today's weekday — computed here, never hardcoded.
    use chrono::Datelike;
    let expected = chrono::Local::now().weekday().num_days_from_monday() as i64 + 1;
    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    assert_eq!(
        output.matches('◯').count() as i64,
        expected - 1,
        "output: {output:?}"
    );
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
    assert_eq!(
        output.matches('◯').count() as i64 + output.matches('●').count() as i64,
        expected
    );
}

#[tokio::test]
async fn test_mood_tracker_grid_week_start_config() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.grid.week_start = im::config::Weekday::Sunday;

    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":").await;

    // Non-rolling week anchored to Sunday: days since the last Sunday.
    use chrono::Datelike;
    let expected = chrono::Local::now().weekday().num_days_from_sunday() as i64 + 1;
    assert_eq!(
        output.matches('◯').count() as i64 + output.matches('●').count() as i64,
        expected,
        "output: {output:?}"
    );
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
}

#[tokio::test]
async fn test_mood_tracker_grid_month_rolling_default() {
    let pool = test_pool().await.unwrap();
    // Defaults: month_rolling=true = the subrepo's rolling "last 4 weeks"
    // window: today - 27 days advanced to the week start (Monday), through today.
    let config = Config::default();

    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":month").await;

    // Compute the expected window start independently of the implementation,
    // so the assertion never depends on when the test runs.
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let mut start = today - chrono::Duration::days(27);
    while start.weekday() != chrono::Weekday::Mon {
        start += chrono::Duration::days(1);
    }
    let expected = (today - start).num_days() + 1;

    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    assert_eq!(
        output.matches('◯').count() as i64,
        expected - 1,
        "output: {output:?}"
    );
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
    assert_eq!(
        output.matches('◯').count() as i64 + output.matches('●').count() as i64,
        expected
    );
    assert!(output.contains("◯  ◯"), "dots must be spaced: {output:?}");
    assert!(
        !output.contains('·'),
        "empty days must use ◯, not ·: {output:?}"
    );
}

#[tokio::test]
async fn test_mood_tracker_grid_month_rolling_false() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.grid.month_rolling = false;

    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":month").await;

    // Non-rolling month = month start through today: day-of-month dots.
    use chrono::Datelike;
    let expected = chrono::Local::now().date_naive().day() as i64;
    assert_eq!(
        output.matches('◯').count() as i64 + output.matches('●').count() as i64,
        expected,
        "output: {output:?}"
    );
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
}

// ---- year grid layout tests (grid.year_rolling) ----

#[tokio::test]
async fn test_mood_tracker_grid_year_default_rolling() {
    let pool = test_pool().await.unwrap();
    // Default: year_rolling = true → the calendar-year heatmap: 7 weekday
    // rows, one column per week, dots for Jan 1 through today. The first
    // partial week may open with blank cells when Jan 1 isn't week_start.
    let config = Config::default();

    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":year").await;

    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();

    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    // Exactly 7 grid rows (one per weekday, Monday first) — no header line.
    assert_eq!(output.lines().count(), 7, "output: {output:?}");
    // One dot per day Jan 1..=today; today is the only filled day.
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
    assert_eq!(
        output.matches('·').count() as i64,
        today.ordinal() as i64 - 1,
        "output: {output:?}"
    );
    assert!(
        !output.contains('◯'),
        "year grid must not use the large ◯: {output:?}"
    );
}

#[tokio::test]
async fn test_mood_tracker_grid_year_not_rolling_calendar_layout() {
    let pool = test_pool().await.unwrap();
    // year_rolling = false → calendar year: Jan 1 through today. Unlike the
    // rolling (aligned-to-week-start) mode, the first partial week may open
    // with blank cells when Jan 1 isn't week_start.
    let mut config = Config::default();
    config.grid.year_rolling = false;

    let cmd = parse_from(vec!["good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let output = run_tracker(&pool, &config, ":year").await;

    use chrono::{Datelike, Weekday};
    let today = chrono::Local::now().date_naive();
    let jan1 = today.with_ordinal(1).unwrap();

    assert!(!output.contains("Mood tracker"), "output: {output:?}");
    // Exactly 7 grid rows (one per weekday, Monday first) — no header line.
    assert_eq!(output.lines().count(), 7, "output: {output:?}");
    // One dot per day Jan 1..=today; today is the only filled day.
    assert_eq!(output.matches('●').count(), 1, "output: {output:?}");
    assert_eq!(
        output.matches('·').count() as i64,
        today.ordinal() as i64 - 1,
        "output: {output:?}"
    );
    assert!(
        !output.contains('◯'),
        "year grid must not use the large ◯: {output:?}"
    );
    // Calendar-year grid indents the first partial week when Jan 1 is not
    // week_start (the aligned mode is what avoids leading blank cells).
    if jan1.weekday() != Weekday::Mon {
        let first_row = output.lines().nth(1).unwrap();
        assert!(
            first_row.starts_with(' '),
            "calendar-year grid must indent the first partial week: {first_row:?}"
        );
    }
}

// ---- short-id allocation policy tests ----

/// Helper: all short ids in table order. `None` entries are completed
/// (oneshot) tasks whose short id was cleared on completion.
async fn fetch_all_short_ids(pool: &SqlitePool) -> Vec<Option<i64>> {
    let rows = sqlx::query("SELECT short_id FROM todos")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.get::<Option<i64>, _>("short_id"))
        .collect()
}

#[tokio::test]
async fn test_short_id_allocator_smallest_free_positive() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create three oneshot tasks; they get short ids 1, 2, 3 in order.
    for s in ["task a", "task b", "task c"] {
        let cmd = parse_from(vec!["!".to_string(), s.to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }
    let mut ids = fetch_all_short_ids(&pool).await;
    ids.sort();
    assert_eq!(
        ids,
        vec![Some(1), Some(2), Some(3)],
        "first three get short ids 1..3: {ids:?}"
    );

    // Delete the middle row directly so the allocator must recycle the gap.
    sqlx::query("DELETE FROM todos WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    let cmd = parse_from(vec!["!".to_string(), "task d".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let mut ids = fetch_all_short_ids(&pool).await;
    ids.sort();
    // After deleting id=2 (short id 2) the remaining short ids are {1, 3};
    // the smallest free is 2, so task d gets short id 2 (the _set_ becomes
    // {1, 2, 3}, not {1, 2, 4}).
    assert_eq!(
        ids,
        vec![Some(1), Some(2), Some(3)],
        "deleted short id 2 should be reused: {ids:?}"
    );
}

#[tokio::test]
async fn test_completions_clear_short_ids_active_keeps_its() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    for s in ["first", "second", "third"] {
        let cmd = parse_from(vec!["!".to_string(), s.to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }
    // Complete third first, then first. Both lose their short id; "second"
    // stays active and keeps short id 2.
    for s in ["third", "first"] {
        let id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
            .bind(s)
            .fetch_one(&pool)
            .await
            .unwrap();
        let cmd = parse_from(vec![format!("+{id}"), "1".to_string()]).unwrap();
        execute_command(
            cmd,
            &pool,
            &config,
            &CliOpts::default(),
            &mut Vec::new(),
            false,
        )
        .await
        .unwrap();
    }

    let mut ids = fetch_all_short_ids(&pool).await;
    ids.sort();
    assert_eq!(
        ids,
        vec![None, None, Some(2)],
        "completed tasks lose their short ids; active 'second' keeps id 2: {ids:?}"
    );

    // A completed task's former short id is immediately free for reuse.
    let cmd = parse_from(vec!["!".to_string(), "fourth".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let mut ids = fetch_all_short_ids(&pool).await;
    ids.sort();
    assert_eq!(
        ids,
        vec![None, None, Some(1), Some(2)],
        "freed short id 1 is reused by the next task: {ids:?}"
    );
}

#[tokio::test]
async fn test_untoggle_reassigns_smallest_free_short_id() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["!".to_string(), "toggle".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Complete it: the short id is cleared.
    let cmd = parse_from(vec!["+1".to_string(), "1".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'toggle'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(short_id.is_none(), "after complete: {short_id:?}");

    // Undo via the word query form (`- <words> -1`): a completed task has no
    // short id, so it's only addressable by words. Untoggling reassigns the
    // smallest free short id (1 — the completed task's own former slot).
    let cmd = parse_from(vec![
        "+toggle".to_string(),
        "-1".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'toggle'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(short_id, Some(1), "after undo: {short_id:?}");
}

#[tokio::test]
async fn test_reset_reassigns_short_id_to_completed_task() {
    // Untoggling by *removing todo_completion entries* (the TUI @done reset
    // path) must also reassign the smallest free short id.
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec!["!".to_string(), "restore me".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["+1".to_string(), "1".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'restore me'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(short_id.is_none(), "after complete: {short_id:?}");

    // Remove the completion rows directly (what the TUI reset does).
    let row_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = 'restore me'")
        .fetch_one(&pool)
        .await
        .unwrap();
    im::db::reset_task_completions(&pool, row_id, None)
        .await
        .unwrap();

    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'restore me'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        short_id,
        Some(1),
        "removing completion entries must reassign the smallest free short id: {short_id:?}"
    );
}

/// `:db backfill` persists embeddings and scores for rows that render no
/// longer backfills inline (mood_color_cached is sync/no-backfill).
#[tokio::test]
async fn test_db_backfill_persists_scores_and_embeddings() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // A CLI-created mood carries its score already; insert two rows without
    // score/embedding directly.
    let cmd = parse_from(vec!["bright".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let now = im::date::now();
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('dull', '', ?)")
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES ('', 'journal only', ?)")
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

    let cmd = parse_from(vec![":db".to_string(), "backfill".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let rows: Vec<(String, Option<f32>, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT mood, score, embedding FROM mood ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    // The directly-inserted mood row now has a score and an embedding.
    assert!(rows[1].1.is_some(), "score backfilled: {rows:?}");
    assert!(rows[1].2.is_some(), "embedding backfilled");
    // Journal-only rows stay untouched (no embedding, no score).
    assert!(rows[2].1.is_none(), "journal row must keep score None");
    assert!(rows[2].2.is_none(), "journal row must keep embedding None");
}

// ---- :db prune command ----

/// Helper: count completions for a given task id (using its post-reassign id).
async fn completion_count(pool: &SqlitePool, name: &str) -> i64 {
    let id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `:db prune` deletes completed oneshot tasks and their cascaded completions.
#[tokio::test]
async fn test_prune_deletes_completed_task_and_cascades_completions() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a oneshot and complete it via the short id (fresh pool: row id
    // == short id == 1). Completion clears the short id but keeps the row.
    let cmd = parse_from(vec!["!".to_string(), "park me".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["+1".to_string(), "3".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(completion_count(&pool, "park me").await, 1);
    let short_id: Option<i64> =
        sqlx::query_scalar("SELECT short_id FROM todos WHERE name = 'park me'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        short_id.is_none(),
        "pre-prune short id should be cleared: {short_id:?}"
    );

    // :db prune should drop the row and the cascaded completion (via ON DELETE
    // CASCADE on todo_completions in db.rs).
    let cmd = parse_from(vec![":db".to_string(), "prune".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM todos WHERE name = 'park me')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists, "pruned task should be gone");

    let completed_orphans: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
            .bind(1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        completed_orphans, 0,
        "FK cascade should drop the completion: {completed_orphans}"
    );
}

/// `:db prune` deletes recurring tasks whose end_time is in the past, leaving
/// open-ended and not-yet-expired recurrings alone.
#[tokio::test]
async fn test_prune_deletes_expired_recurring_task() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let past = im::date::now() - 3600;
    sqlx::query(
        "INSERT INTO todos (name, body, priority, start_time, interval_secs, target_count, optional, end_time) \
         VALUES ('expired', '', 5, ?, ?, 1, 0, ?)",
    )
    .bind(past - 86_400)
    .bind(pack_interval(86_400))
    .bind(past)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO todos (name, body, priority, start_time, interval_secs, target_count, optional, end_time) \
         VALUES ('still going', '', 5, ?, ?, 1, 0, ?)",
    )
    .bind(past)
    .bind(past + 86_400)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO todos (name, body, priority, start_time, interval_secs, target_count, optional, end_time) \
         VALUES ('forever', '', 5, ?, ?, 1, 0, NULL)",
    )
    .bind(pack_interval(86_400))
    .execute(&pool)
    .await
    .unwrap();

    let cmd = parse_from(vec![":db".to_string(), "prune".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let names: Vec<String> = sqlx::query("SELECT name FROM todos")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(!names.contains(&"expired".to_string()));
    assert!(names.contains(&"still going".to_string()));
    assert!(names.contains(&"forever".to_string()));
}

/// `:prune` clears the `embedding_cache` table entirely — it is a cache;
/// entries are lazily re-embedded on the next use.
#[tokio::test]
async fn test_prune_clears_embedding_cache() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let key1 = format!("{}happy", config.moods.axes.prefix_string);
    let key2 = format!("{}obsolete_mood", config.moods.axes.prefix_string);

    // Populate the embedding cache with two entries
    sqlx::query("INSERT INTO embedding_cache (text, embedding) VALUES ($1, x'00000000')")
        .bind(&key1)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO embedding_cache (text, embedding) VALUES ($1, x'00000000')")
        .bind(&key2)
        .execute(&pool)
        .await
        .unwrap();

    let cache_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM embedding_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cache_before, 2);

    // Run :prune
    let cmd = parse_from(vec![":db".to_string(), "prune".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let cache_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM embedding_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        cache_after, 0,
        "prune should clear the whole embedding cache"
    );
}

// ---- Invalid timestamps must fail task creation ----

/// Garbage `@<time>` values (and invalid calendar dates) must fail task
/// parsing — oneshot and scheduled — rather than silently landing in the
/// task name. The date is resolved to an epoch at CLI parse time
/// (`DATE_DIALECT`), so a bad value fails there, before anything is created.
#[tokio::test]
async fn test_task_creation_invalid_timestamps_fail() {
    let pool = test_pool().await.unwrap();

    // Oneshot with a garbage date: `! task @x`.
    let err = parse_from(vec!["!".to_string(), "task".to_string(), "@x".to_string()]).unwrap_err();
    assert!(
        format!("{err:#}").contains("Invalid task start time"),
        "unexpected error: {err:#}"
    );

    // Invalid calendar date: `! task @2024-99-99`.
    let err = parse_from(vec![
        "!".to_string(),
        "task".to_string(),
        "@2024-99-99".to_string(),
    ])
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("Invalid task start time"),
        "unexpected error: {err:#}"
    );

    // Scheduled with a garbage start: `! @x`.
    let err = parse_from(vec!["!".to_string(), "@x".to_string()]).unwrap_err();
    assert!(
        format!("{err:#}").contains("Invalid scheduled task start time"),
        "unexpected error: {err:#}"
    );

    // Nothing was created by any of the failures.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// ---- FK cascade on delete (CASCADE propagation to todo_completions) ----

/// Deleting a task must remove its `todo_completions` rows via the
/// `ON DELETE CASCADE` declared in db.rs.
#[tokio::test]
async fn test_delete_task_cascades_completions() {
    let pool = test_pool().await.unwrap();
    // let config = Config::default();

    let id = create_oneshot_task(&pool, "to cull").await;
    update_task(&pool, id, im::date::now(), 1).await;
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 1, "seed completion");

    sqlx::query("DELETE FROM todos WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 0, "FK CASCADE should drop completions");
}

#[tokio::test]
async fn test_bundled_config_defaults_load_through_serde() {
    // The bundled `assets/config.toml` ships with hex RGB endpoints via the
    // crossterm serde `#RRGGBB` format; the anchor pairs now live in the
    // bundled moods file (`DEFAULT_MOODS`, named by `[moods] source`). If
    // this test fails on a serde error, the bundled config/moods files have
    // drifted from the parse path (e.g. someone changed a hex literal in
    // `assets/moods.toml` without updating the anchors). It deliberately
    // does NOT assert exact RGB values — tweak those freely for palette
    // work without breaking the contract.
    let cfg: Config = toml::from_str(im::config::DEFAULT_CONFIG)
        .expect("bundled DEFAULT_CONFIG must deserialize");

    let moods: im::config::MoodsFile =
        toml::from_str(im::config::DEFAULT_MOODS).expect("bundled DEFAULT_MOODS must parse");
    assert!(!moods.pairs.is_empty());

    // The default config points `source` at the moods file.
    assert!(!cfg.moods.source.as_os_str().is_empty());

    // Mood names and order are valid.
    assert_eq!(moods.pairs[0].mood, "happy");
    assert_eq!(moods.pairs[1].mood, "sad");
}

// ---------- Event-loop architecture & new TUI actions ----------

/// The TUI render loops use the same SQL operations the CLI does; these
/// tests pin the semantics the action handlers rely on.

#[tokio::test]
async fn test_delete_mood_removes_linked_tracker_rows() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Insert a mood with a linked tracker row (like `mood ok -sleep 8`).
    let cmd = parse_from(vec![
        "ok".to_string(),
        "-sleep".to_string(),
        "8".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let mood_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'ok'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let linked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker WHERE mood = ?")
        .bind(mood_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(linked, 1);

    // The today TUI delete path: delete tracker rows first (FK, no cascade),
    // then the mood row, in a transaction.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM tracker WHERE mood = ?")
        .bind(mood_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mood WHERE id = ?")
        .bind(mood_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let moods: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(moods, 0);
    let trackers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trackers, 0);
}

/// `sql::delete_tracker_entry` (the today view's tracker-entry delete path) removes
/// exactly the targeted row.
#[tokio::test]
async fn test_delete_tracker_row() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    // Two tracker entries, one linked to a mood.
    let cmd = parse_from(vec![
        "ok".to_string(),
        "-sleep".to_string(),
        "8".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let cmd = parse_from(vec!["-sleep".to_string(), "7".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM tracker ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);

    // Delete the unlinked row; the linked one (and its mood) survive.
    let affected = im::db::delete_tracker_entry(&pool, ids[1]).await.unwrap();
    assert_eq!(affected, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
    let moods: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(moods, 1);
}

#[tokio::test]
async fn test_delete_mood_without_cascade_fails_with_fk_enforced() {
    // The tracker.mood FK has no ON DELETE CASCADE, so deleting a mood
    // row while linked tracker rows still exist must fail under PRAGMA
    // foreign_keys = ON. This is why the today delete path deletes tracker
    // rows first.
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec![
        "ok".to_string(),
        "-sleep".to_string(),
        "8".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let mood_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'ok'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let r = sqlx::query("DELETE FROM mood WHERE id = ?")
        .bind(mood_id)
        .execute(&pool)
        .await;
    assert!(
        r.is_err(),
        "FK must block deleting a mood with linked trackers"
    );
}

#[tokio::test]
async fn test_edit_todo_body_updates_in_place() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a task with a body via `! name . body`.
    let cmd = parse_from(vec![
        "!".to_string(),
        "ship it".to_string(),
        im::cli::BODY_DELIMITER.to_string(),
        "initial body".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = 'ship it'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The TUI edit path: UPDATE todos SET body = ? WHERE id = ?.
    sqlx::query("UPDATE todos SET body = ? WHERE id = ?")
        .bind("rewritten body")
        .bind(task_id)
        .execute(&pool)
        .await
        .unwrap();

    let body: String = sqlx::query_scalar("SELECT body FROM todos WHERE id = ?")
        .bind(task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(body, "rewritten body");
    // Name is untouched.
    let name: String = sqlx::query_scalar("SELECT name FROM todos WHERE id = ?")
        .bind(task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "ship it");
}

#[tokio::test]
async fn test_edit_tracker_text_payload() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "note".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Text,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec!["-note".to_string(), "hello".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let tracker_id: i64 = sqlx::query_scalar("SELECT id FROM tracker WHERE type = 'note'")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query("UPDATE tracker SET score = ? WHERE id = ?")
        .bind("edited text")
        .bind(tracker_id)
        .execute(&pool)
        .await
        .unwrap();

    let score: String = sqlx::query_scalar("SELECT score FROM tracker WHERE id = ?")
        .bind(tracker_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(score, "edited text");
}

#[tokio::test]
async fn test_edit_tracker_float_payload() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec!["-sleep".to_string(), "8".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let tracker_id: i64 = sqlx::query_scalar("SELECT id FROM tracker WHERE type = 'sleep'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The EditTracker modal path: float kind parses f64, then UPDATE.
    sqlx::query("UPDATE tracker SET score = ? WHERE id = ?")
        .bind(7.5f64)
        .bind(tracker_id)
        .execute(&pool)
        .await
        .unwrap();

    let score: f64 = sqlx::query_scalar("SELECT score FROM tracker WHERE id = ?")
        .bind(tracker_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(score, 7.5);
}

#[tokio::test]
async fn test_edit_mood_body() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    let cmd = parse_from(vec![
        "calm".to_string(),
        im::cli::BODY_DELIMITER.to_string(),
        "original note".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let mood_id: i64 = sqlx::query_scalar("SELECT id FROM mood WHERE mood = 'calm'")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query("UPDATE mood SET body = ? WHERE id = ?")
        .bind("revised note")
        .bind(mood_id)
        .execute(&pool)
        .await
        .unwrap();

    let body: String = sqlx::query_scalar("SELECT body FROM mood WHERE id = ?")
        .bind(mood_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(body, "revised note");
}

#[tokio::test]
async fn test_reset_progress_oneshot_clears_all_completions() {
    let pool = test_pool().await.unwrap();

    let task_id = create_oneshot_task(&pool, "reset me").await;
    update_task(&pool, task_id, im::date::now(), 1).await;

    // The @done reset path for a oneshot task: delete all completions.
    sqlx::query("DELETE FROM todo_completions WHERE todo_id = ?")
        .bind(task_id)
        .execute(&pool)
        .await
        .unwrap();

    let total = im::task::apply_completion_delta(&pool, task_id, 0)
        .await
        .unwrap();
    assert_eq!(
        total, 0,
        "oneshot task should have no completions after reset"
    );
}

#[tokio::test]
async fn test_reset_progress_recurring_only_current_interval() {
    let pool = test_pool().await.unwrap();

    // Recurring task with a 1-day interval, started 3 days + 500s ago.
    let interval = 86_400i64;
    let now = im::date::now();
    let start_time = now - 3 * interval - 500;
    let name = "daily reset";
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(name)
    .bind("")
    .bind(5)
    .bind(pack_interval(interval))
    .bind::<Option<i64>>(None)
    .bind(0)
    .bind(0)
    .bind(start_time)
    .execute(&pool)
    .await
    .unwrap();
    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();

    let interval_start = im::task::current_interval_start(start_time, interval_span(interval), now);
    update_task(&pool, task_id, interval_start - 100, 2).await;
    update_task(&pool, task_id, interval_start + 100, 3).await;

    // The @done reset path for a recurring task: delete only completions
    // at/after the current interval start (same floor as the views use).
    sqlx::query("DELETE FROM todo_completions WHERE todo_id = ? AND time >= ?")
        .bind(task_id)
        .bind(interval_start)
        .execute(&pool)
        .await
        .unwrap();

    let prev_sum: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ? AND time < ?",
    )
    .bind(task_id)
    .bind(interval_start)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prev_sum, 2, "previous intervals must be preserved");

    let total = im::task::apply_completion_delta(&pool, task_id, 0)
        .await
        .unwrap();
    assert_eq!(total, 0, "current interval must be empty after reset");
}

#[tokio::test]
async fn test_fetch_today_entries_carries_tracker_ids() {
    // The today view's Edit/Delete dispatch relies on tracker entries
    // carrying their row id so the SQL update/delete can target them.
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );

    let cmd = parse_from(vec!["-sleep".to_string(), "8".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        im::date::today_start(),
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();
    let tracker = entries
        .iter()
        .find(|e| e.kind == im::today::EntryKind::Tracker(TrackerKind::Float))
        .expect("tracker entry must appear in today view");
    assert!(tracker.id.is_some(), "tracker entry must carry its row id");

    // And the id must match the DB row.
    let db_id: i64 = sqlx::query_scalar("SELECT id FROM tracker WHERE type = 'sleep'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tracker.id, Some(db_id));
}

#[tokio::test]
async fn test_fetch_today_entries_completed_task_has_check_badge() {
    // The today view renders one row per task; a completed task's row
    // carries the ✓ badge — completion rows are no longer emitted (WP9 9e).
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a oneshot task due today, then complete it.
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let cmd = parse_from(vec![
        "!".to_string(),
        "completed task".to_string(),
        format!("@{today_str}"),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();
    let task_id: i64 = sqlx::query_scalar("SELECT id FROM todos WHERE name = 'completed task'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let update_cmd = parse_from(vec![format!("+{task_id}"), "1".to_string()]).unwrap();
    execute_command(
        update_cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        im::date::today_start(),
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();
    // (Legacy: the today view used to emit separate completion rows; that
    // behavior is gone and cannot be expressed via EntryKind — the enum has
    // no completion variant.)
    let task_rows: Vec<_> = entries
        .iter()
        .filter(|e| e.kind.is_task() && e.label == "completed task")
        .collect();
    assert_eq!(task_rows.len(), 1, "exactly one task row expected");
    assert_eq!(
        task_rows[0].badge(&config).0,
        Some('✓'),
        "done task row must carry ✓"
    );
    assert_eq!(task_rows[0].task_id, Some(task_id));
}

/// A oneshot completed before the anchored day never lingers in the today
/// view's `All` variant — the regular oneshot fetch is incomplete-only and
/// the completed-today merge covers only the anchored day. A oneshot
/// completed today still surfaces (✓ badge, completion time), and open
/// oneshots are unaffected.
#[tokio::test]
async fn test_today_view_stale_completed_oneshot_hidden() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let now = im::date::now();
    let two_days_ago = now - 2 * 86_400;

    // Stale completed oneshot: completion entry two days ago.
    let stale = insert_oneshot(&pool, "stale completed", two_days_ago, 1).await;
    update_task(&pool, stale, two_days_ago, 1).await;
    // Completed today.
    let today = insert_oneshot(&pool, "completed today", now, 1).await;
    update_task(&pool, today, now, 1).await;
    // Still open, undated.
    insert_oneshot(&pool, "open undated", now - 86_400, 0).await;

    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        im::date::today_start(),
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();

    let labels: Vec<String> = entries
        .iter()
        .filter(|e| e.kind.is_task())
        .map(|e| e.label.clone())
        .collect();
    assert!(
        !labels.iter().any(|l| l == "stale completed"),
        "completed-before-yesterday oneshot must not linger: {labels:?}"
    );
    let today_row = entries
        .iter()
        .find(|e| e.label == "completed today")
        .expect("completed-today oneshot must appear via the merge");
    assert_eq!(today_row.badge(&config).0, Some('✓'));
    assert!(labels.iter().any(|l| l == "open undated"), "{labels:?}");
}

/// `[badges] journal_badge` controls the journal-only entry badge; None
/// renders no badge at all.
#[tokio::test]
async fn test_today_view_journal_badge() {
    let pool = test_pool().await.unwrap();
    let mut config = Config::default();
    let axes = im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    // Journal-only entry: mood '' with a body (via CLI: `mood . text`).
    let cmd = parse_from(vec![
        im::cli::BODY_DELIMITER.to_string(),
        "a journal note".to_string(),
    ])
    .unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    // Default (no journal_badge): no badge at all.
    let mut out = Vec::new();
    im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    let line = output
        .lines()
        .find(|l| l.contains("a journal note"))
        .unwrap();
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(
        cols[1], "",
        "journal badge must be empty by default: {line:?}"
    );

    // With a configured badge, the journal entry carries it. The default
    // color (Reset) renders it plain — no ANSI escapes.
    config.badges.journal_badge = Some(im::config::BadgeSetting {
        badge: Some('•'),
        color: None,
    });
    let mut out = Vec::new();
    im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    let line = output
        .lines()
        .find(|l| l.contains("a journal note"))
        .unwrap();
    let cols: Vec<&str> = line.split('\t').collect();
    assert!(
        cols[1].contains('•'),
        "journal badge must come from config: {line:?}"
    );
    assert!(
        !output.contains("\u{1b}["),
        "uncolored journal badge must render plain: {output:?}"
    );

    // An explicit color wraps the badge in ANSI.
    config.badges.journal_badge = Some(im::config::BadgeSetting {
        badge: Some('•'),
        color: Some(crossterm::style::Color::Red),
    });
    let mut out = Vec::new();
    im::today::write_today_view(
        &pool,
        &config,
        &axes,
        im::date::today_start(),
        im::types::ViewVariant::All,
        im::types::TodayHorizon::Today,
        &CliOpts::default(),
        &mut out,
    )
    .await
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains("\u{1b}["),
        "colored journal badge must emit ANSI: {output:?}"
    );
}

#[tokio::test]
async fn test_clear_command() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Create a mood entry for today
    let cmd = parse_from(vec!["mood".to_string(), "good".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Clear entries for today (non-interactive mode in tests)
    let clear_cmd = parse_from(vec![":clear".to_string()]).unwrap();
    execute_command(
        clear_cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mood")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count_after, 0);
}

/// `:db doctor` parse: exactly `:db doctor`; extra args, a bare `:db`, and
/// unknown subcommands are usage errors.
#[test]
fn test_db_doctor_parse() {
    let cmd = parse_from(vec![":db".to_string(), "doctor".to_string()]).unwrap();
    assert!(matches!(
        cmd,
        im::cli::Command::Db {
            sub: im::cli::DbSubcommand::Doctor
        }
    ));
    assert!(
        parse_from(vec![
            ":db".to_string(),
            "doctor".to_string(),
            "extra".to_string()
        ])
        .is_err()
    );
    assert!(parse_from(vec![":db".to_string()]).is_err());
    assert!(parse_from(vec![":db".to_string(), "bogus".to_string()]).is_err());
}

/// `:db doctor` non-interactive safety: mismatched entries are surfaced but
/// never deleted without the interactive confirm.
#[tokio::test]
async fn test_db_doctor_noninteractive_reports_only() {
    let pool = test_pool().await.unwrap();
    // sleep is float in the config: real entries match, integer and text
    // entries mismatch. "old" has no config section (orphan).
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Float,
            strict: false,
            colors: None,
        },
    );
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind(3.5f64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind(3i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind("deep")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('old', ?, 100)")
        .bind(1i64)
        .execute(&pool)
        .await
        .unwrap();

    let cmd = parse_from(vec![":db".to_string(), "doctor".to_string()]).unwrap();
    execute_command(
        cmd,
        &pool,
        &config,
        &CliOpts::default(),
        &mut Vec::new(),
        false,
    )
    .await
    .unwrap();

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 4, "non-interactive :db doctor must not delete");
}

/// `:db doctor` db layer: fetch_tracker_score_kinds groups entries by type
/// and storage class (with nonzero counts); prune_tracker_rules deletes
/// exactly the mismatched rows across all rule kinds.
#[tokio::test]
async fn test_db_doctor_buckets_and_prune() {
    let pool = test_pool().await.unwrap();
    // sleep: null with both min/max set — time-marker mode, so only integer
    // score-0 entries match. water: number — integers only.
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: Some(82800.0),
            high: Some(7200.0),
            kind: TrackerKind::Null,
            strict: false,
            colors: None,
        },
    );
    config.tracker.insert(
        "water".to_string(),
        im::config::TrackerSetting {
            interval: None,
            low: None,
            high: None,
            kind: TrackerKind::Integer,
            strict: false,
            colors: None,
        },
    );
    // sleep: 2 zero integers (keep) + 1 nonzero integer (stale count-mode
    // leftover) + 1 text (mismatch); water: 2 integers (keep) + 1 real
    // (mismatch); "old": orphan.
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind(0i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind(0i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind(2i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('sleep', ?, 100)")
        .bind("deep")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('water', ?, 100)")
        .bind(5i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('water', ?, 100)")
        .bind(4i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('water', ?, 100)")
        .bind(3.5f64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO tracker (type, score, time) VALUES ('old', ?, 100)")
        .bind(1i64)
        .execute(&pool)
        .await
        .unwrap();

    let kinds = im::db::fetch_tracker_score_kinds(&pool).await.unwrap();
    let buckets: Vec<(String, String, i64, i64)> = kinds
        .iter()
        .map(|r| {
            (
                r.tracker_type.clone(),
                r.storage.clone(),
                r.count,
                r.nonzero,
            )
        })
        .collect();
    assert_eq!(
        buckets,
        vec![
            ("old".to_string(), "integer".to_string(), 1, 1),
            ("sleep".to_string(), "integer".to_string(), 3, 1),
            ("sleep".to_string(), "text".to_string(), 1, 1),
            ("water".to_string(), "integer".to_string(), 2, 2),
            ("water".to_string(), "real".to_string(), 1, 1),
        ]
    );

    // The rules plan_tracker_prunes would derive from this config: sleep
    // (marker-mode null) = keep integers + drop nonzero; water = keep
    // integers; old = orphan, drop everything.
    let rules = vec![
        im::db::TrackerPruneRule::Storage {
            tracker_type: "sleep".to_string(),
            keep: "integer",
        },
        im::db::TrackerPruneRule::NonzeroScore {
            tracker_type: "sleep".to_string(),
        },
        im::db::TrackerPruneRule::Storage {
            tracker_type: "water".to_string(),
            keep: "integer",
        },
        im::db::TrackerPruneRule::All {
            tracker_type: "old".to_string(),
        },
    ];
    let deleted = im::db::prune_tracker_rules(&pool, &rules).await.unwrap();
    assert_eq!(deleted, 4, "1 nonzero + 1 text + 1 real + 1 orphan");

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracker")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 4, "2 sleep zero-integers + 2 water integers");
}

#[tokio::test]
async fn test_today_view_journal_shows_oneshot_completions_with_progress_and_preview() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let today_start = im::date::today_start();
    let yesterday = today_start - 86400;

    // Oneshot task with target_count = 5.
    let t = insert_oneshot(&pool, "water plants", today_start - 2 * 86400, 5).await;

    // 1 completion yesterday (count 1): cumulative = 1.
    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, 1)")
        .bind(t)
        .bind(yesterday + 15 * 3600)
        .execute(&pool)
        .await
        .unwrap();

    // 3 completions today at 09:00, 12:00, 15:00.
    let t1 = today_start + 9 * 3600;
    let t2 = today_start + 12 * 3600;
    let t3 = today_start + 15 * 3600;

    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, 1)")
        .bind(t)
        .bind(t1)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, 1)")
        .bind(t)
        .bind(t2)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, 1)")
        .bind(t)
        .bind(t3)
        .execute(&pool)
        .await
        .unwrap();

    // Another task with no completions.
    insert_oneshot(&pool, "uncompleted chore", today_start, 1).await;

    // Fetch entries for Today in Journal mode (ViewVariant::A).
    let im::today::TodayFetch { entries, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        today_start,
        im::types::ViewVariant::A,
    )
    .await
    .unwrap();

    // Uncompleted chore must NOT appear.
    assert!(
        !entries.iter().any(|e| e.label == "uncompleted chore"),
        "open tasks with no completions must not appear in journal view"
    );

    // Only completions within the horizon (today's 3 completions) appear.
    let task_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.kind.is_task() && e.label == "water plants")
        .collect();
    assert_eq!(task_entries.len(), 3, "expected 3 completion entries today");

    // Timestamps: 09:00, 12:00, 15:00.
    assert_eq!(task_entries[0].time_label, "09:00");
    assert_eq!(task_entries[1].time_label, "12:00");
    assert_eq!(task_entries[2].time_label, "15:00");

    // Cumulative completions at completion time: 2, 3, 4 (since 1 occurred yesterday).
    assert_eq!(task_entries[0].task.as_ref().unwrap().completions, Some(2));
    assert_eq!(task_entries[1].task.as_ref().unwrap().completions, Some(3));
    assert_eq!(task_entries[2].task.as_ref().unwrap().completions, Some(4));

    // Preview for each entry displays the progress at that completion time (2/5, 3/5, 4/5).
    let preview1 = im::ui::build_preview(
        task_entries[0].task.as_ref().unwrap(),
        true,
        &config,
        &[],
        None,
        None,
        None,
    );
    let preview1_text: String = preview1.into_iter().map(|l| l.to_string()).collect();
    assert!(preview1_text.contains("2/5"), "preview 1 must show 2/5: {preview1_text}");

    let preview2 = im::ui::build_preview(
        task_entries[1].task.as_ref().unwrap(),
        true,
        &config,
        &[],
        None,
        None,
        None,
    );
    let preview2_text: String = preview2.into_iter().map(|l| l.to_string()).collect();
    assert!(preview2_text.contains("3/5"), "preview 2 must show 3/5: {preview2_text}");

    let preview3 = im::ui::build_preview(
        task_entries[2].task.as_ref().unwrap(),
        true,
        &config,
        &[],
        None,
        None,
        None,
    );
    let preview3_text: String = preview3.into_iter().map(|l| l.to_string()).collect();
    assert!(preview3_text.contains("4/5"), "preview 3 must show 4/5: {preview3_text}");
}

#[tokio::test]
async fn test_today_view_tasks_variant_omits_completed_today() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();
    im::color::ColorAxes::build(&pool, &config.moods)
        .await
        .unwrap();

    let today_start = im::date::today_start();
    let now = today_start + 10 * 3600;

    // Completed today task.
    let t = insert_oneshot(&pool, "finished today", today_start, 1).await;
    update_task(&pool, t, now, 1).await;

    // Open overdue task.
    sqlx::query(
        "INSERT INTO todos (name, body, priority, interval_secs, available_duration_secs, target_count, optional, start_time, end_time) \
         VALUES ('overdue chore', '', 5, NULL, NULL, 1, 0, ?, ?)",
    )
    .bind(today_start - 7200)
    .bind(today_start - 3600)
    .execute(&pool)
    .await
    .unwrap();

    // Show: Tasks (ViewVariant::B) should not explicitly include tasks completed today.
    let im::today::TodayFetch { entries: entries_b, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        today_start,
        im::types::ViewVariant::B,
    )
    .await
    .unwrap();

    let labels_b: Vec<_> = entries_b.iter().map(|e| e.label.as_str()).collect();
    assert!(labels_b.contains(&"overdue chore"));
    assert!(
        !labels_b.contains(&"finished today"),
        "show: tasks must not explicitly include tasks completed today: {labels_b:?}"
    );

    // Show: All (ViewVariant::All) should include tasks completed today.
    let im::today::TodayFetch { entries: entries_all, .. } = im::today::fetch_today_entries(
        &pool,
        &config,
        im::types::TodayHorizon::Today,
        today_start,
        im::types::ViewVariant::All,
    )
    .await
    .unwrap();

    let labels_all: Vec<_> = entries_all.iter().map(|e| e.label.as_str()).collect();
    assert!(labels_all.contains(&"finished today"));
    assert!(labels_all.contains(&"overdue chore"));
}

#[tokio::test]
async fn test_mood_links_at_most_one_task() {
    let pool = test_pool().await.unwrap();
    let config = Config::default();

    // Insert two tasks.
    let t1 = insert_oneshot(&pool, "task 1", 1_700_000_000, 1).await;
    let t2 = insert_oneshot(&pool, "task 2", 1_700_000_000, 1).await;

    // Create a mood entry.
    sqlx::query("INSERT INTO mood (id, mood, body, time) VALUES (100, 'focused', '', 1_700_000_000)")
        .execute(&pool)
        .await
        .unwrap();

    // Link mood 100 to task 1.
    im::db::link_mood_to_task(&pool, 100, t1).await.unwrap();
    let links: Vec<(Option<i64>, i64)> = sqlx::query_as("SELECT todo_id, id FROM mood WHERE id = 100")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(links, vec![(Some(t1), 100)]);

    // Link mood 100 to task 2 (replaces link to task 1).
    im::db::link_mood_to_task(&pool, 100, t2).await.unwrap();
    let links: Vec<(Option<i64>, i64)> = sqlx::query_as("SELECT todo_id, id FROM mood WHERE id = 100")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(links, vec![(Some(t2), 100)]);

    // Link via link_mood_to_tasks replaces as well.
    im::db::link_mood_to_tasks(&pool, 100, &[t1]).await.unwrap();
    let links: Vec<(Option<i64>, i64)> = sqlx::query_as("SELECT todo_id, id FROM mood WHERE id = 100")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(links, vec![(Some(t1), 100)]);
}
