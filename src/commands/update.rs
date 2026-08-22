//! Completion updates for oneshot tasks: applied by the entry handler when
//! an entry's `+<task_ref>` carries a count payload (see
//! `crate::commands::entry`).

use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::db::TaskUpdateInfo;

/// Apply a completion increment to a oneshot task and print the result.
/// `info.id` is the stable row id; `info.short_id` is the user-facing id as
/// it was before the update. `update_task` syncs the short id to the
/// completion state, so a done → not-done transition reassigns the smallest
/// free id — the not-done message re-reads it so it reflects the post-update
/// id.
pub(super) async fn update_oneshot(
    pool: &SqlitePool,
    opts: &CliOpts,
    info: &TaskUpdateInfo,
    count: Option<i32>,
) -> Result<()> {
    let increment = count.unwrap_or(1);
    let new_completions = crate::db::update_task(pool, info.id, increment).await?;
    let is_done = crate::task::is_task_done(info.target_count, Some(new_completions));

    if !opts.quiet() {
        if is_done {
            println!(
                "Task '{}' completed! (completions: {})",
                info.name, new_completions
            );
        } else {
            let short_id = crate::db::fetch_task_short_id(pool, info.id).await?;
            println!(
                "Task '{}' (id {}) updated: {}/{} completions",
                info.name,
                short_id.unwrap_or_default(),
                new_completions,
                info.target_count
            );
        }
    }

    Ok(())
}
