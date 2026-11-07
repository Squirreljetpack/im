use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::Id;
use crate::types::TaskRef;

/// Resolve an `Id`/`Words` task ref to its stable row id. `Pick` needs a
/// terminal, so the caller owns the picker and resolves it before calling.
pub(crate) async fn resolve_task_ref(pool: &SqlitePool, task_ref: &TaskRef) -> Result<Id> {
    match task_ref {
        TaskRef::Id(short_id) => crate::db::fetch_task_id_by_short_id(pool, *short_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No task with short id {short_id} exists")),
        TaskRef::Words(words) => {
            let matches = crate::db::fetch_task_matching_words(pool, words).await?;
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
