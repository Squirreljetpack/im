use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::{CliOpts, UpdateTarget};
use crate::db::TaskUpdateInfo;

pub(super) async fn update_task_command(
    pool: &SqlitePool,
    opts: &CliOpts,
    target: UpdateTarget,
    count: Option<i32>,
) -> Result<()> {
    match target {
        UpdateTarget::OneShot(short_id) => {
            // `im - <id> [count]`: the id is the user-facing short id
            // (see sql.rs). Completed tasks have no short id and are not
            // addressable here — use the word query form instead.
            let info = crate::db::fetch_oneshot_task_for_update(pool, short_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Oneshot task with id {} not found", short_id))?;
            update_oneshot(pool, opts, &info, count).await?
        }
        UpdateTarget::Query { words } => {
            // `im - <words…> [count]`: update the *unique* oneshot task
            // whose name contains the words in their order. Zero matches and
            // multiple matches both fail — the caller must disambiguate.
            let matches = crate::db::fetch_oneshot_matching_words(pool, &words).await?;
            let joined = words.join(" ");
            match matches.len() {
                0 => anyhow::bail!(
                    "No task matches \"{}\" — the words must appear in a task name, in order",
                    joined
                ),
                1 => update_oneshot(pool, opts, &matches[0], count).await?,
                n => {
                    let names = matches
                        .iter()
                        .map(|m| match m.short_id {
                            Some(sid) => format!("'{}' (id {})", m.name, sid),
                            None => format!("'{}' (completed)", m.name),
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::bail!(
                        "{} tasks match \"{}\": {}. Use more words or the task id",
                        n,
                        joined,
                        names
                    )
                }
            }
        }
    }

    Ok(())
}

/// Apply a completion increment to a oneshot task. `info.id` is the stable
/// row id; `info.short_id` is the user-facing id as it was before the update.
/// `update_task` syncs the short id to the completion state, so a done →
/// not-done transition reassigns the smallest free id — the not-done message
/// re-reads it so it reflects the post-update id.
async fn update_oneshot(
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
