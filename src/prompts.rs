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

/// Prompt for a task's start time with custom label. Blank input falls back to the default:
/// `Some(default)` (recurring creation — the placeholder shows the formatted
/// `default`) or `now` for scheduled creation. Validated against the fixed
/// `crate::date::DATE_DIALECT` so a bad time fails before the task is created.
pub fn prompt_start_time(label: &str, default: Option<&str>) -> Result<i64> {
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

    let raw: String = input(label)
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

/// Prompt for an optional parent task reference (`+<id>` / `+<words>` / `+` / blank-none).
pub fn prompt_parent_id() -> Result<Option<crate::types::TaskRef>> {
    use cliclack::input;

    let raw: String = input("(Optional) Parent:")
        .placeholder("none")
        .default_input("")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed == "+" {
        return Ok(Some(crate::types::TaskRef::Pick));
    }
    let without_plus = trimmed.strip_prefix('+').unwrap_or(trimmed).trim();
    if without_plus.is_empty() {
        return Ok(Some(crate::types::TaskRef::Pick));
    }
    // All-digits input is always an id reference — never a word query —
    // even when no task holds that id (e.g. `+0`).
    if without_plus.chars().all(|c| c.is_ascii_digit()) {
        let n = without_plus
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("Invalid task id: '{without_plus}'"))?;
        return Ok(Some(crate::types::TaskRef::Id(n)));
    }
    let words = without_plus
        .split_whitespace()
        .map(String::from)
        .collect::<Vec<_>>();
    Ok(Some(crate::types::TaskRef::Words(words)))
}

/// Prompt for a task's user-facing short id in the editor.
pub async fn prompt_short_id(
    pool: &sqlx::SqlitePool,
    current_id: i64,
    current_short: Option<i64>,
) -> Result<Option<i64>> {
    use cliclack::input;

    let placeholder = current_short
        .map(|s| s.to_string())
        .unwrap_or_else(|| "none".to_string());

    loop {
        let raw: String = input("Short ID:")
            .placeholder(&placeholder)
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

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(current_short);
        }
        let new_short = trimmed.parse::<i64>()?;
        if Some(new_short) == current_short {
            return Ok(current_short);
        }
        if let Some((existing_id, _)) =
            crate::db::fetch_task_id_by_short_id(pool, new_short).await?
            && existing_id != current_id
        {
            cliclack::log::error(format!("Short ID {} is already in use", new_short))?;
            continue;
        }
        return Ok(Some(new_short));
    }
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
/// input resolves in three passes — a calendar-aware span first (`"1 year"`,
/// `"3 months"`, relative to now), then a fixed duration (`"90d"`), then an
/// absolute date/time via the fixed `crate::date::DATE_DIALECT`.
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
            if input.is_empty() || input == "never" {
                return Ok(());
            }
            resolve_end_input(input, default_time)
                .map(|_| ())
                .map_err(|e| format!("Invalid duration or time: {}", e))
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;

    if raw.is_empty() || raw == "never" {
        Ok(default_time)
    } else {
        resolve_end_input(&raw, default_time)
    }
}

/// Resolve an end-time input: calendar-aware span relative to now first
/// (`"1 year"`), then a fixed duration (`"90d"`), then an absolute
/// date/time. Blank input is the caller's concern; `default_time` is
/// returned for `"never"`.
fn resolve_end_input(raw: &str, default_time: Option<i64>) -> Result<Option<i64>> {
    if raw == "never" {
        return Ok(default_time);
    }
    // Calendar spans need a reference date to resolve to seconds.
    if let Ok(span) = crate::date::parse_span(raw) {
        let end = crate::date::zoned_from_unix_secs(crate::date::now())
            .and_then(|z| z.checked_add(span))
            .map_err(anyhow::Error::msg)?;
        return Ok(Some(end.timestamp().as_second()));
    }
    if let Ok(dur) = crate::date::parse_duration_secs(raw) {
        return Ok(Some(crate::date::now() + dur));
    }
    crate::date::parse_datetime(raw, crate::date::DATE_DIALECT).map(Some)
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

#[cfg(test)]
mod tests {
    use super::resolve_end_input;

    /// End-time input resolves in three passes: calendar-aware span first
    /// (`"1 year"`), then fixed duration, then absolute date/time. Blank
    /// and "never" are handled by the caller/prompt, not here.
    #[test]
    fn end_input_span_duration_datetime_precedence() {
        // Calendar span (rejected by the plain duration parser) resolves
        // relative to now.
        let end = resolve_end_input("1 year", None).unwrap().unwrap();
        assert!(end > crate::date::now() + 86_400 * 364);
        assert!(end < crate::date::now() + 86_400 * 367);

        // Fixed duration still works ("90d"; a span of 90 calendar days
        // matches the same input first — allow for a DST shift).
        let end = resolve_end_input("90d", None).unwrap().unwrap();
        assert!(
            (end - (crate::date::now() + 90 * 86_400)).abs() <= 3600,
            "got {end}"
        );

        // Absolute date/time wins last.
        let ts = crate::date::parse_datetime("2030-03-15", crate::date::DATE_DIALECT).unwrap();
        let end = resolve_end_input("2030-03-15", None).unwrap().unwrap();
        assert_eq!(end, ts);

        // "never" falls back to the caller's default (None here).
        assert_eq!(resolve_end_input("never", None).unwrap(), None);
        assert_eq!(resolve_end_input("never", Some(123)).unwrap(), Some(123));

        // Garbage fails all three passes.
        assert!(resolve_end_input("nonsense", None).is_err());
    }
}
