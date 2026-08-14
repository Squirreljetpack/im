use sqlx::SqlitePool;

use super::completion::{apply_completion_delta, is_task_done};
use super::scheduling::interval_start;

/// What the Accept action should do with a task — decided by the shared pure
/// fn so both TUIs behave identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptAction {
    /// Mark done — scheduled: set 1; other: +1.
    Complete,
    /// Scheduled only: set 0 (failed).
    SetFailed,
    /// Scheduled Some(0) before the window end: remove the entry.
    Clear,
    /// `target_count <= 1` done: reset directly, no modal.
    Reset,
    /// `target_count > 1` done: the TUI asks "Reset progress?" (default Yes).
    ResetConfirm,
    /// `target_count > 1` not done: the TUI opens the numeric CompleteModal.
    CompletePrompt,
}

/// Next Accept-action for a task, pure and testable. Scheduled tasks never
/// prompt: they cycle 1 → 0 → (none | 1) with `before` = window still open
/// (`start + duration >= now`; other kinds are always "before").
pub fn accept_action(
    completions: Option<i32>,
    is_scheduled: bool,
    target_count: i32,
    start_time: Option<i64>,
    duration: Option<i64>,
    now: i64,
) -> AcceptAction {
    if is_scheduled {
        match completions {
            None => AcceptAction::Complete,
            Some(c) if c > 0 => AcceptAction::SetFailed,
            Some(_) => {
                let before = start_time.unwrap_or(now) + duration.unwrap_or(0) >= now;
                if before {
                    AcceptAction::Clear
                } else {
                    AcceptAction::Complete
                }
            }
        }
    } else if is_task_done(target_count, completions) {
        if target_count <= 1 {
            AcceptAction::Reset
        } else {
            AcceptAction::ResetConfirm
        }
    } else if target_count <= 1 {
        AcceptAction::Complete
    } else {
        AcceptAction::CompletePrompt
    }
}

/// Execute a modal-less [`AcceptAction`] against the database. `ResetConfirm`
/// and `CompletePrompt` are TUI-side (modals) and unreachable here.
pub async fn apply_accept_action(
    pool: &SqlitePool,
    task: &crate::db::TaskRow,
    action: AcceptAction,
) -> anyhow::Result<()> {
    match action {
        AcceptAction::Complete => {
            if task.is_scheduled() {
                crate::db::set_scheduled_completion(pool, task.id, 1).await?;
            } else {
                apply_completion_delta(pool, task.id, 1).await?;
            }
        }
        AcceptAction::SetFailed => {
            crate::db::set_scheduled_completion(pool, task.id, 0).await?;
        }
        AcceptAction::Clear => {
            crate::db::reset_task_completions(pool, task.id, None).await?;
        }
        AcceptAction::Reset => reset_task_progress(pool, task).await?,
        AcceptAction::ResetConfirm | AcceptAction::CompletePrompt => {
            unreachable!("modal actions are handled by the TUI")
        }
    }
    Ok(())
}

/// Reset a task's completion progress: scheduled/oneshot clear all
/// completions; recurring resets only the current interval (earlier
/// history survives).
pub async fn reset_task_progress(
    pool: &SqlitePool,
    task: &crate::db::TaskRow,
) -> anyhow::Result<()> {
    let floor = interval_start(task, crate::date::now());
    crate::db::reset_task_completions(pool, task.id, floor).await?;
    Ok(())
}
