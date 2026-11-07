use anyhow::Result;
use sqlx::SqlitePool;
use std::io::Write;

use crate::config::Config;
use crate::task::{completed_sort_time, pending_sort_time};
use crate::types::{ViewMode, ViewVariant};

/// Handle a task view (non-terminal output): writes tab-separated rows to the
/// writer. TUI dispatch is handled by [`crate::commands::execute_command`].
pub async fn write_task_view<W: Write>(
    pool: &SqlitePool,
    mode: ViewMode,
    config: &Config,
    show: ViewVariant,
    out: &mut W,
) -> Result<()> {
    let mut tasks = crate::db::fetch_tasks_for_view(
        pool,
        mode,
        show,
        config.tasks_view.persist_pending_seconds,
    )
    .await?;

    // CLI ordering uses the same date keys as the TUIs: pending views sort
    // priority descending with `task_entry_time` (date ascending) as the
    // fallback; the done view sorts by `task_done_time` (last completion
    // entry, else start + duration) newest first. The SQL ORDER BY only
    // provides a deterministic base for equal keys.
    let now = crate::date::now();
    if mode == ViewMode::DoneTasks {
        // Date sort: done time, newest first.
        tasks.sort_by_key(|t| std::cmp::Reverse(completed_sort_time(t)));
    } else {
        // Priority sort with the date key as fallback (ascending).
        tasks.sort_by_key(|t| (std::cmp::Reverse(t.priority), pending_sort_time(t, now)));
    }

    if tasks.is_empty() {
        writeln!(out, "No tasks found for view: {:?}", mode)?;
        return Ok(());
    }

    write!(
        out,
        "{}",
        crate::output::format_tasks_simple(&tasks, config, mode == ViewMode::DoneTasks)
    )?;

    Ok(())
}
