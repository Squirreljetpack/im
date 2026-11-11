//! Completion updates for referenced tasks: applied by the entry handler for
//! an entry's `+<task_ref>` — the explicit count, or the implicit one from
//! `tasks.increment_cli_ref_by_default` (see `crate::commands::entry`).

use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::db::TaskRow;

/// Apply a completion delta to an oneshot or recurring task and print the
/// result. Recurring rows scope the delta to the current interval
/// (`db::update_task`).
///
/// `task.id` is the stable row id; `task.short_id` is the user-facing id as it
/// was before the update. `update_task` syncs the short id to the completion
/// state, so a done → not-done transition reassigns the smallest free id —
/// the not-done message re-reads it so it reflects the post-update id.
pub(super) async fn apply_completion_delta(
    pool: &SqlitePool,
    opts: &CliOpts,
    task: &TaskRow,
    delta: i32,
) -> Result<()> {
    let new_completions = crate::db::update_task(pool, task.id, delta).await?;
    let is_done = crate::task::is_task_done(task.target_count, Some(new_completions));

    if !opts.quiet() {
        if is_done {
            println!(
                "Task '{}' completed! (completions: {})",
                task.name, new_completions
            );
        } else {
            let short_id = crate::db::fetch_task_short_id(pool, task.id).await?;
            println!(
                "Task '{}' (id {}) updated: {}/{} completions",
                task.name,
                short_id.unwrap_or_default(),
                new_completions,
                task.target_count
            );
        }
    }

    Ok(())
}

/// Set a scheduled task's state from a CLI count. A scheduled row's completion
/// row holds the state itself rather than a counter, so the counts are
/// discrete: `1` done, `2` failed, `-1` unmarked; `0` changes nothing (the
/// count semantics of every other kind).
pub(super) async fn set_scheduled_state(
    pool: &SqlitePool,
    opts: &CliOpts,
    task: &TaskRow,
    count: i32,
) -> Result<()> {
    let (value, state) = match count {
        0 => return Ok(()),
        1 => (Some(1), "completed"),
        2 => (Some(0), "marked failed"),
        -1 => (None, "unmarked"),
        other => anyhow::bail!(
            "Scheduled task '{}' takes 1 (done), 2 (failed) or -1 (unmarked), got {other}",
            task.name
        ),
    };
    crate::db::set_scheduled_completion(pool, task.id, value).await?;
    if !opts.quiet() {
        println!("Task '{}' {state}", task.name);
    }
    Ok(())
}
