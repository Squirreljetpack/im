use crossterm::style::Color;
use serde::{Deserialize, Serialize};

use super::types::ColorBins;

/// `[tasks]` section — defaults for new tasks and the completion-badge
/// colors.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasksConfig {
    /// Default priority (1–999) for new oneshot tasks.
    pub default_priority: i32,
    /// Default priority for new recurring tasks.
    pub default_recurring_priority: i32,
    /// Default priority for new scheduled tasks.
    pub default_scheduled_priority: i32,
    /// Colors for the completion badge (`◯`/`●`) shown in task lists, from
    /// lowest to highest progress.
    pub colors: ColorBins,
    /// Color for overdue task labels: the today view's date column and the
    /// preview's `due:` field of an overdue oneshot task (see
    /// `badge::task_label_color`). Pale pink by default.
    pub overdue_color: Color,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            default_priority: 10,
            default_recurring_priority: 5,
            default_scheduled_priority: 15,
            colors: Default::default(),
            // Pale pink (#FFB6C1).
            overdue_color: Color::Rgb {
                r: 0xFF,
                g: 0xB6,
                b: 0xC1,
            },
        }
    }
}
