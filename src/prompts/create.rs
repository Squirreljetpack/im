//! Prompts for the TUI creation menu (`ctrl-n`): the target picker and the
//! entry/session values. The flows that write the chosen target live in
//! [`crate::commands::create`].

use anyhow::Result;

/// The creation targets offered by the create menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateKind {
    /// A mood or journal entry.
    Mood,
    /// A tracker entry (the tracker TUI picks the tracker).
    TrackerEntry,
    /// A timed mood session.
    Session,
    /// A oneshot task.
    Oneshot,
    /// A recurring task.
    Recurring,
    /// A scheduled task.
    Scheduled,
}

/// Prompt for one creation target. `include_entries` adds the mood and
/// tracker-entry choices (today view); the tasks view passes `false`.
pub fn prompt_create_kind(include_entries: bool) -> Result<CreateKind> {
    let mut select = cliclack::select("Create:");
    if include_entries {
        select = select
            .item(
                CreateKind::Mood,
                "Mood",
                "log a mood, journal entry, or both",
            )
            .item(
                CreateKind::TrackerEntry,
                "Tracker entry",
                "pick a tracker and log a value",
            );
    }
    let kind = select
        .item(CreateKind::Session, "Timed session", "run a mood session")
        .item(CreateKind::Oneshot, "Oneshot task", "")
        .item(CreateKind::Recurring, "Recurring task", "")
        .item(CreateKind::Scheduled, "Scheduled task", "")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;
    Ok(kind)
}

/// Prompt for a mood or journal line. Empty is allowed (a journal entry).
pub fn prompt_mood() -> Result<String> {
    use cliclack::input;

    let mood: String = input("Mood:")
        .placeholder("empty for a journal entry")
        .default_input("")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;
    Ok(mood.trim().to_string())
}

/// Prompt for a single-line body. Empty is allowed.
pub fn prompt_body() -> Result<String> {
    use cliclack::input;

    let body: String = input("Body:")
        .placeholder("empty")
        .default_input("")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;
    Ok(body.trim().to_string())
}

/// Prompt for a mood session's duration. The answer is required and must be
/// a positive duration (e.g. `25m`).
pub fn prompt_session_duration() -> Result<i64> {
    use cliclack::input;

    let raw: String = input("Session duration:")
        .placeholder("25m")
        .default_input("")
        .validate(|value: &String| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(String::from("Duration is required"))
            } else {
                match crate::date::parse_duration_secs(trimmed) {
                    Ok(secs) if secs > 0 => Ok(()),
                    Ok(_) => Err(String::from("Duration must be positive")),
                    Err(e) => Err(format!("Invalid duration: {}", e)),
                }
            }
        })
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {}", e))?;
    crate::date::parse_duration_secs(raw.trim())
}
