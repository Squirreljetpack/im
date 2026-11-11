use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::{Id, TaskRow};
use crate::types::{TaskKind, TaskRef};

/// Whether a word ref may name this task: the query names one kind and the
/// task has to be that kind (`@` recurring, `%` scheduled, plain oneshot).
fn matches_kind(task: &TaskRow, kind: TaskKind) -> bool {
    task.kind() == kind
}

/// Resolve an `Id`/`Words` task ref to its stable row id. `Pick` needs a
/// terminal, so the caller owns the picker and resolves it before calling.
pub(crate) async fn resolve_task_ref(pool: &SqlitePool, task_ref: &TaskRef) -> Result<Id> {
    match task_ref {
        TaskRef::Id(short_id) => crate::db::fetch_task_id_by_short_id(pool, *short_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No task with short id {short_id} exists")),
        TaskRef::Words { words, kind } => {
            let matches: Vec<TaskRow> = crate::db::fetch_task_matching_words(pool, words)
                .await?
                .into_iter()
                .filter(|task| matches_kind(task, *kind))
                .collect();
            match matches.len() {
                0 => anyhow::bail!("No task found matching query '{}'", words.join(" ")),
                1 => Ok(matches[0].id),
                n => anyhow::bail!(
                    "Multiple tasks match query '{}' (found {})",
                    words.join(" "),
                    n
                ),
            }
        }
        TaskRef::Pick => anyhow::bail!("Picking a task requires an interactive terminal"),
    }
}

/// Resolve an entry's `+<ref>` to the task it may link to, restricted to
/// [`crate::db::entry_ref_addressable`] rows. Candidates are filtered before
/// the ambiguity check, so a task the entry cannot refer to never shadows the
/// live one that matches the same words.
pub(crate) async fn resolve_entry_task_ref(
    pool: &SqlitePool,
    task_ref: &TaskRef,
    now: i64,
) -> Result<TaskRow> {
    match task_ref {
        TaskRef::Id(short_id) => {
            let task = crate::db::fetch_task_by_short_id(pool, *short_id, now)
                .await?
                .ok_or_else(|| anyhow::anyhow!("No task with short id {short_id} exists"))?;
            gate(task, now)
        }
        TaskRef::Words { words, kind } => {
            let query = words.join(" ");
            let matches = crate::db::fetch_task_matching_words(pool, words).await?;
            let matches = crate::db::attach_full_completions(pool, matches, now).await?;
            let addressable: Vec<TaskRow> = matches
                .into_iter()
                .filter(|task| matches_kind(task, *kind))
                .filter(|task| crate::db::entry_ref_addressable(task, now))
                .collect();
            if addressable.is_empty() {
                anyhow::bail!("No task found matching query '{query}'");
            }
            if let [only] = addressable.as_slice() {
                return Ok(only.clone());
            }
            // Several oneshots: only the one still open can be acted on, so it
            // is the unambiguous reading. Other kinds stay ambiguous.
            if kind == &TaskKind::Oneshot
                && let Some(task) = sole_unfinished_oneshot(&addressable)
            {
                return Ok(task.clone());
            }
            anyhow::bail!(
                "Multiple tasks match query '{query}' (found {})",
                addressable.len()
            )
        }
        TaskRef::Pick => anyhow::bail!("Picking a task requires an interactive terminal"),
    }
}

/// Pick the task an ambiguous oneshot query means: exactly one of the matches
/// is unfinished, so it is the only one an entry can act on. Two unfinished
/// oneshots stay ambiguous.
fn sole_unfinished_oneshot(matches: &[TaskRow]) -> Option<&TaskRow> {
    let mut unfinished = matches.iter().filter(|task| !task.is_done());
    let only = unfinished.next()?;
    unfinished.next().is_none().then_some(only)
}

/// Resolve the id the interactive picker returned to its row, applying the
/// same gate: the pending view already lists addressable tasks, but a row can
/// be completed while the picker is open.
pub(crate) async fn entry_task_by_id(pool: &SqlitePool, id: Id, now: i64) -> Result<TaskRow> {
    let task = crate::db::fetch_task_by_id(pool, id, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Task not found"))?;
    gate(task, now)
}

/// Refuse a task an entry cannot refer to. Only a recurring task outside its
/// availability window fails the gate.
fn gate(task: TaskRow, now: i64) -> Result<TaskRow> {
    if crate::db::entry_ref_addressable(&task, now) {
        return Ok(task);
    }
    anyhow::bail!(
        "Recurring task '{}' is not inside its current availability window",
        task.name
    )
}
