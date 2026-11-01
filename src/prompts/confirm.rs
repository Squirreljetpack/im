//! Confirmations for destructive or irreversible operations.

use anyhow::Result;

/// Confirm deleting `count` mood entries for `date`.
pub fn prompt_clear_confirm(count: i64, date: &str) -> Result<bool> {
    use cliclack::confirm;

    confirm(format!("Clear {} mood entry/entries for {}?", count, date))
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}

/// Confirm pruning kind-mismatched tracker entries (`:db doctor`). Default
/// is `false`: the deletion is destructive and covers whole tracker types.
pub fn prompt_db_doctor_confirm(count: i64) -> Result<bool> {
    use cliclack::confirm;

    confirm(format!("Prune {count} mismatched tracker entry/entries?"))
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}

/// Confirm resetting the local database so `im :sync --reset` can rebuild
/// the local database from the server log.
pub fn prompt_discard_unsynced(count: usize) -> Result<bool> {
    use cliclack::{confirm, intro};

    intro("Reset local database")?;
    cliclack::log::warning(format!(
        "{count} local change(s) have not been synced and will be discarded."
    ))?;
    confirm("Rebuild the local database from the server log?".to_string())
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}
