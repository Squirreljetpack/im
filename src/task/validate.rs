//! Write-time invariants for task rows.
//!
//! The edit menus keep these true by construction, but the write seam is the
//! only check every caller shares.

use anyhow::{Result, bail};

use crate::db::UpdateTaskObject;
use crate::types::TaskKind;

/// Reject an edit that would break what every reader assumes about a row.
///
/// A task's kind is derived from which of `interval_secs` and
/// `available_duration_secs` are set ([`TaskKind::of`]), so clearing one of
/// them does not change a field — it silently reclassifies the task, and with
/// it its availability math, view membership and completion scoping.
/// Transitions are rejected rather than reinterpreted.
pub fn validate_edit(before: TaskKind, update: &UpdateTaskObject) -> Result<()> {
    let after = TaskKind::of(update.interval_secs, update.available_duration_secs);
    if after != before {
        bail!("A {before} task cannot become a {after} task");
    }
    if update.interval_secs.is_some_and(|secs| secs <= 0) {
        bail!("A recurring task's interval must be positive");
    }
    if update.available_duration_secs.is_some_and(|secs| secs <= 0) {
        bail!("An available duration must be positive");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(
        interval_secs: Option<i64>,
        available_duration_secs: Option<i64>,
    ) -> UpdateTaskObject {
        UpdateTaskObject {
            id: crate::db::Id::new(),
            short_id: None,
            name: "task".to_string(),
            body: String::new(),
            priority: 0,
            start_time: 1_700_000_000,
            available_duration_secs,
            interval_secs,
            target_count: 0,
            optional: false,
            end_time: None,
            parent: None,
        }
    }

    #[test]
    fn rejects_kind_transitions_and_nonpositive_spans() {
        // The kind is derived, so clearing its column reclassifies the task.
        assert!(validate_edit(TaskKind::Scheduled, &update(None, None)).is_err());
        assert!(validate_edit(TaskKind::Recurring, &update(None, Some(60))).is_err());

        // Intervals and durations are positive.
        assert!(validate_edit(TaskKind::Recurring, &update(Some(0), None)).is_err());
        assert!(validate_edit(TaskKind::Scheduled, &update(None, Some(0))).is_err());

        assert!(validate_edit(TaskKind::Recurring, &update(Some(86_400), None)).is_ok());
        assert!(validate_edit(TaskKind::Scheduled, &update(None, Some(60))).is_ok());
        assert!(validate_edit(TaskKind::Oneshot, &update(None, None)).is_ok());
    }
}
