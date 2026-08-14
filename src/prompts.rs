//! Interactive prompts (cliclack) for the task and clear flows.
//!
//! Every interactive input/confirm in the CLI lives here. Prompts are pure
//! UI: no SQL, no file I/O. Flow-level banner and feedback output lives in
//! `crate::output`; database access lives in `crate::db`.

use anyhow::Result;
use std::path::Path;

/// Maximum allowed task priority. Anything higher is rejected at ingestion
/// time (cliclack validation in `prompt_priority`). Lower bound is 1 — zero
/// and negative priorities are not meaningful.
pub const MAX_PRIORITY: i32 = 999;

/// Prompt for a task priority. Blank input falls back to `default`.
pub fn prompt_priority(default: i32) -> Result<i32> {
    use cliclack::input;

    let raw: String = input("Priority:")
        .placeholder(&default.to_string())
        .default_input("")
        .validate(|value: &String| {
            if value.is_empty() {
                Ok(())
            } else {
                match value.parse::<i32>() {
                    Ok(n) if (1..=MAX_PRIORITY).contains(&n) => Ok(()),
                    Ok(_) => Err(format!("Priority must be between 1 and {}", MAX_PRIORITY)),
                    Err(_) => Err(String::from("Must be a number")),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(raw
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|n| (1..=MAX_PRIORITY).contains(n))
        .unwrap_or(default))
}

/// Prompt for a task's start time. Blank input falls back to the default:
/// `Some(default)` (recurring creation — the placeholder shows the formatted
/// `default`) or `now` for scheduled creation. Validated against the fixed
/// `crate::date::DATE_DIALECT` so a bad time fails before the task is created.
pub fn prompt_start_time(default: Option<&str>) -> Result<i64> {
    use cliclack::input;

    let (default_time, placeholder) = match default {
        Some(s) => (
            crate::date::parse_datetime(s, crate::date::DATE_DIALECT)
                .expect("Placeholder should parse"),
            s.to_string(),
        ),
        None => {
            let now = crate::date::now();
            (now, crate::date::format_datetime(now))
        }
    };

    let raw: String = input("Start time:")
        .placeholder(&placeholder)
        .default_input("")
        .validate(move |input: &String| {
            if input.trim().is_empty() {
                Ok(())
            } else {
                crate::date::parse_datetime(input, crate::date::DATE_DIALECT)
                    .map(|_| ())
                    .map_err(|e| format!("Invalid time: {}", e))
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    if raw.trim().is_empty() {
        Ok(default_time)
    } else {
        crate::date::parse_datetime(&raw, crate::date::DATE_DIALECT)
    }
}

/// Prompt for a task completion target. Blank input falls back to `default`
/// (0 = task can be completed once). The caller picks the label, e.g.
/// `"Times to complete (0 = once):"` for creation or
/// `"Times to complete per interval (blank = once):"` for edit.
pub fn prompt_target_count() -> Result<i32> {
    use cliclack::input;

    let raw: String = input("Times to complete:")
        .placeholder("0 (once)")
        .default_input("0")
        .validate(|value: &String| {
            if value.is_empty() {
                Ok(())
            } else {
                match value.parse::<i32>() {
                    Ok(n) if n >= 0 => Ok(()),
                    Ok(_) => Err(String::from("Must be non-negative")),
                    Err(_) => Err(String::from("Must be a number")),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(raw.trim().parse::<i32>().unwrap_or(0))
}

/// Prompt for an optional parent task short id (`! -<parent_id>`). Blank
/// input means no parent. The short id is not validated against the
/// database here (that needs the pool — the caller resolves it and errors
/// on an unknown id).
pub fn prompt_parent_id() -> Result<Option<i64>> {
    use cliclack::input;

    let raw: String = input("(Optional) Parent id:")
        .placeholder("none")
        .default_input("")
        .validate(|value: &String| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(())
            } else {
                match trimmed.parse::<i64>() {
                    Ok(n) if n > 0 => Ok(()),
                    Ok(_) => Err(String::from("Must be positive")),
                    Err(_) => Err(String::from("Must be a number")),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(raw.trim().parse::<i64>().ok())
}

/// Confirm attaching the new task under `name` as its parent. The parent id
/// was typed explicitly, so the default is yes — this is a double-check
/// that the id names the intended task.
pub fn prompt_attach_parent(name: &str) -> Result<bool> {
    use cliclack::confirm;

    confirm(format!("Attach to task: {}?", name))
        .initial_value(true)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}

/// Prompt for a task name (required, no tabs, trimmed). The duplicate-name
/// check against the database is the caller's responsibility (it needs the
/// pool); `prefill` seeds the input for `im ! @ <name>`.
pub fn prompt_name(prefill: Option<&str>) -> Result<String> {
    use cliclack::input;

    let name: String = input("Task name:")
        .placeholder("exercise")
        .default_input(prefill.unwrap_or(""))
        .validate(|input: &String| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                Err("Name is required")
            } else if trimmed.contains('\t') {
                Err("Name cannot contain tab characters")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(name.trim().to_string())
}

/// Prompt for a recurrence interval (required, valid duration). `default`
/// pre-fills the input (edit flow).
pub fn prompt_interval(default: Option<&str>) -> Result<String> {
    use cliclack::input;

    let raw: String = input("Interval between task occurrences:")
        .placeholder("1 day 2 hours")
        .default_input(default.unwrap_or(""))
        .validate(|input: &String| {
            if input.is_empty() {
                Err(String::from("Interval is required"))
            } else {
                match crate::date::parse_span(input) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("Invalid duration: {}", e)),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(raw)
}

/// Prompt for an available duration. Blank = always available. `max` (in
/// seconds) optionally caps the allowed input: a duration longer than
/// `format_duration(max)` is rejected (the recurring flow passes the
/// interval, since availability beyond it means always available). `default`
/// pre-fills the input (edit flow).
pub fn prompt_available_duration(
    placeholder: &str,
    default: Option<&str>,
    max: Option<i64>,
) -> Result<String> {
    use cliclack::input;

    let raw: String = input("Available duration:")
        .placeholder(placeholder)
        .default_input(default.unwrap_or(""))
        .validate(move |input: &String| {
            if input.is_empty() {
                Ok(())
            } else {
                match crate::date::parse_duration_secs(input) {
                    Ok(secs) => match max {
                        Some(m) if secs > m => Err(format!(
                            "Must be at most {}",
                            crate::date::format_duration(m)
                        )),
                        _ => Ok(()),
                    },
                    Err(e) => Err(format!("Invalid duration: {}", e)),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    Ok(raw)
}

/// Prompt for a recurring-task end time. The label is "Duration or end time":
/// input is validated as a duration first (e.g. "1 year", relative to now)
/// and then as an absolute date/time via the fixed `crate::date::DATE_DIALECT`.
/// Blank = never ends.
/// Returns the resolved Unix-epoch end time, or `None` for never. `default`
/// is the remaining time in seconds (pre-filled as a formatted duration).
pub fn prompt_end(default: Option<&str>) -> Result<Option<i64>> {
    use cliclack::input;

    let (default_time, placeholder) = match default {
        Some(s) => (
            Some(
                crate::date::parse_datetime(s, crate::date::DATE_DIALECT)
                    .expect("Placeholder should parse"),
            ),
            s.to_string(),
        ),
        None => (None, "never".to_string()),
    };

    let raw: String = input("Duration or end time:")
        .placeholder(&placeholder)
        .default_input("")
        .validate(move |input: &String| {
            if input.is_empty()
                || input == "never"
                || crate::date::parse_duration_secs(input).is_ok()
            {
                Ok(())
            } else {
                crate::date::parse_datetime(input, crate::date::DATE_DIALECT)
                    .map(|_| ())
                    .map_err(|e| format!("Invalid duration or time: {}", e))
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    if raw.is_empty() || raw == "never" {
        Ok(default_time)
    } else if let Ok(dur) = crate::date::parse_duration_secs(&raw) {
        Ok(Some(crate::date::now() + dur))
    } else {
        crate::date::parse_datetime(&raw, crate::date::DATE_DIALECT).map(Some)
    }
}

/// Prompt whether a recurring task is optional.
pub fn prompt_optional(initial: bool) -> Result<bool> {
    use cliclack::confirm;

    confirm("Is this task optional?")
        .initial_value(initial)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}

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

/// Confirm deleting an invalid database file so it can be recreated fresh.
/// Interactive callers only — non-interactive runs never reach this. The
/// default is `false`: deleting the db destroys all stored data.
pub fn prompt_delete_invalid_db(_path: &Path) -> Result<bool> {
    use cliclack::{confirm, intro};

    intro("Invalid database")?;

    cliclack::log::warning("This will permanently remove ALL stored data.")?;

    confirm("Database is invalid. Delete it and start fresh?".to_string())
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))
}
