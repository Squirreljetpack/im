use anyhow::Result;

use crate::config::Config;
use crate::db::TaskRow;

/// Banner line for an interactive task flow: cliclack's styled `intro`
/// (writes to stderr, matching the prompts — the interactive flow is always
/// at a TTY).
pub fn task_intro(title: &str) -> Result<()> {
    cliclack::intro(title).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Ordered field/value pairs for a task's tab-aligned display.
/// Recurring-only fields (Interval, Available, Optional, End) are shown only
/// for recurring tasks; scheduled tasks show their Available window and the
/// window start; oneshot tasks show Created (the `start_time` creation
/// moment) and Due (the `end_time` deadline, only when set); Body only when
/// non-empty. The Type value is lowercase (`threshold` when a oneshot task
/// has a target count).
pub(crate) fn task_rows(task: &crate::db::TaskObject) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    rows.push((
        "Type".to_string(),
        if task.is_recurring() {
            "recurring".to_string()
        } else if task.is_scheduled() {
            "scheduled".to_string()
        } else if task.target_count > 0 {
            "threshold".to_string()
        } else {
            "oneshot".to_string()
        },
    ));
    rows.push(("Priority".to_string(), task.priority.to_string()));
    if task.is_recurring() || task.is_scheduled() {
        // Scheduled/recurring: the start is the window start / recurrence
        // anchor.
        if let Some(st) = task.start_time {
            rows.push(("Start".to_string(), crate::date::format_datetime(st)));
        }
    } else {
        // Oneshot: creation time always, due only when an end time was set.
        if let Some(st) = task.start_time {
            rows.push(("Created".to_string(), crate::date::format_datetime(st)));
        }
        if let Some(et) = task.end_time {
            rows.push(("Due".to_string(), crate::date::format_datetime(et)));
        }
    }
    if task.is_recurring() {
        rows.push((
            "Interval".to_string(),
            task.interval_span()
                .map(|span| crate::date::format_span(&span))
                .unwrap_or_default(),
        ));
        rows.push((
            "Available".to_string(),
            match task.available_duration_secs {
                Some(a) => crate::date::format_duration(a),
                None => "Always".to_string(),
            },
        ));
        rows.push((
            "Optional".to_string(),
            if task.optional { "Yes" } else { "No" }.to_string(),
        ));
        rows.push((
            "End".to_string(),
            match task.end_time {
                Some(e) => crate::date::format_datetime(e),
                None => "Never".to_string(),
            },
        ));
    } else if task.is_scheduled() {
        rows.push((
            "Available".to_string(),
            task.available_duration_secs
                .map(crate::date::format_duration)
                .unwrap_or_default(),
        ));
    }
    // The Target row only exists when the task actually has a target count.
    if task.target_count > 0 {
        rows.push(("Target".to_string(), task.target_count.to_string()));
    }
    if !task.body.is_empty() {
        rows.push(("Body".to_string(), task.body.clone()));
    }
    rows
}

/// Format task view rows into tab-separated output text.
///
/// 6 columns: `id \t interval \t next_available \t pri \t name \t status`.
/// Recurring tasks fill `interval` (`format_duration`) and `next_available`
/// (the next interval window start, `format_datetime`); oneshot tasks render
/// a single space in both. `done_view` renders the done-list badge variant
/// (`@done` — scheduled `✓`→`◷`, recurring `✓`→`↻`, see `badge::task_badge`).
pub fn format_tasks_simple(tasks: &[TaskRow], config: &Config, done_view: bool) -> String {
    use crossterm::style::{Color as CtColor, Stylize};

    let mut output = String::new();
    for task in tasks {
        let count = task.completions.unwrap_or(0) as i64;
        let (ch, color) = crate::badge::task_badge(task, config, done_view);
        // Same badge as the TUI: colored dot + plain label.
        let dot = if color == CtColor::Reset {
            ch.to_string()
        } else {
            ch.to_string().with(color).to_string()
        };
        // Progress text (" 2/5") for in-progress target tasks. The text's
        // own leading glyph is stripped (it may differ from the displayed
        // badge — ↻/◷/✓) so the dot column doesn't duplicate it.
        let label = crate::badge::completion_badge_text(count, task.target_count);
        let label = label
            .strip_prefix('◯')
            .or_else(|| label.strip_prefix('●'))
            .unwrap_or("")
            .to_string();
        // Completed tasks have no short id — the id column stays empty.
        let id_cell = if task.is_done() {
            String::new()
        } else {
            task.short_id.map(|s| s.to_string()).unwrap_or_default()
        };
        // Recurring tasks show their interval and the next time they become
        // available (the start of the next interval window); oneshot tasks
        // render a single space in both columns.
        let interval_cell = task
            .interval_span()
            .map(|span| crate::date::format_span(&span))
            .unwrap_or_else(|| " ".to_string());
        let next_available_cell = match (task.start_time, task.interval_span()) {
            (Some(start), Some(span)) if crate::date::span_rough_seconds(span) > 0.0 => {
                let now = crate::date::now();
                let next = if now <= start {
                    start
                } else {
                    // Next interval start = end of the current interval.
                    crate::date::interval_end_unix_secs(start, span, now).unwrap_or(start)
                };
                crate::date::format_datetime(next)
            }
            _ => " ".to_string(),
        };
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}{}\n",
            id_cell, interval_cell, next_available_cell, task.priority, task.name, dot, label,
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use crate::db::TaskObject;

    use super::*;

    #[test]
    fn test_task_rows() {
        // Threshold type for a oneshot with a target; Target row present.
        let task = TaskObject {
            id: Some(1),
            short_id: Some(1),
            name: "pushups".to_string(),
            body: String::new(),
            priority: 5,
            start_time: Some(1700000000),
            available_duration_secs: None,
            interval_secs: None,
            target_count: 20,
            optional: false,
            end_time: None,
            parent: None,
        };
        let rows = task_rows(&task);
        let type_row = rows.iter().find(|(l, _)| l == "Type").unwrap();
        assert_eq!(type_row.1, "threshold");
        assert!(rows.iter().any(|(l, v)| l == "Target" && v == "20"));

        // Plain oneshot: lowercase type, no Target row at target_count 0.
        let mut task = task.clone();
        task.target_count = 0;
        let rows = task_rows(&task);
        assert_eq!(rows[0], ("Type".to_string(), "oneshot".to_string()));
        assert!(!rows.iter().any(|(l, _)| l == "Target"));

        // Recurring and scheduled stay lowercase.
        task.interval_secs = Some(crate::date::span_to_db(&jiff::Span::new().days(1)));
        assert_eq!(task_rows(&task)[0].1, "recurring");
        task.interval_secs = None;
        task.start_time = Some(1700000000);
        task.available_duration_secs = Some(3600);
        // Scheduled is discriminated by available_duration + no interval;
        // construct via is_scheduled helpers used by task_rows.
        assert_eq!(task_rows(&task)[0].1, "scheduled");
    }
}
