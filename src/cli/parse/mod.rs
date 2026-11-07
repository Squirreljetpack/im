use anyhow::Context;

use crate::types::TaskRef;

mod entry;
mod groups;
mod special;
mod task;
mod tracker;
mod view;

pub(super) use entry::parse_entry_command;
pub(super) use special::parse_special_command;
pub(super) use task::{parse_task_command, parse_task_edit_command};
pub(super) use view::parse_view_command;

/// Parse a `+<ref>` token into a [`TaskRef`]: bare `+` picks interactively,
/// `+<digits>` is a short ID, and `+<words>` is a whitespace-split word query.
pub(super) fn parse_task_ref(token: &str) -> anyhow::Result<TaskRef> {
    let rest = token
        .strip_prefix('+')
        .expect("caller guarantees a leading '+'");
    if rest.is_empty() {
        return Ok(TaskRef::Pick);
    }
    if rest.chars().all(|c| c.is_ascii_digit()) {
        let id = rest.parse::<i64>().context("Task ID must be a number")?;
        return Ok(TaskRef::Id(id));
    }
    let words: Vec<String> = rest.split_whitespace().map(String::from).collect();
    if words.is_empty() {
        anyhow::bail!("Task reference query cannot be whitespace-only");
    }
    Ok(TaskRef::Words(words))
}

/// Parse an integer completion delta token (`2`, `-1`). Rejects leading `+`
/// so `+2` stays a task reference rather than a completion count.
pub(super) fn parse_count_token(s: &str) -> Option<i32> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        s.parse::<i32>().ok()
    } else {
        None
    }
}

/// Check if a token looks like a number (integer, float, or out-of-range integer).
/// Strips an optional leading '-' and checks if the remainder is numeric.
pub(super) fn is_numeric_token(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    !s.is_empty()
        && !s.starts_with('+')
        && !s.starts_with('-')
        && s.chars().any(|c| c.is_ascii_digit())
        && (s.chars().all(|c| c.is_ascii_digit())
            || s.parse::<f64>().is_ok_and(|f| f.is_finite()))
}
