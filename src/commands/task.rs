use std::path::PathBuf;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::Config;
use crate::date::format_duration;
use crate::db::TaskObject;
use crate::editor::open_editor_for_body;
use crate::types::{Task, TaskKind};

/// Resolve a `-<parent_id>` short id to its stable row id plus the parent's
/// name; errors when no task holds that short id (a completed oneshot holds
/// `NULL` and is never resolvable).
async fn resolve_parent_named(pool: &SqlitePool, short_id: i64) -> Result<(i64, String)> {
    crate::db::fetch_task_id_by_short_id(pool, short_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("No task with short id {short_id} exists (cannot attach it as parent)")
        })
}

/// Resolve a `-<parent_id>` short id to a stable row id; errors when no
/// task holds that short id (a completed oneshot holds `NULL` and is
/// never resolvable).
async fn resolve_parent(pool: &SqlitePool, short_id: i64) -> Result<Option<i64>> {
    Ok(Some(resolve_parent_named(pool, short_id).await?.0))
}

/// Resolve a task's body text at creation time. The body delimiter (any
/// arg of only dots) is always legal and resolves the same in every flow
/// (direct and interactive):
///
/// * `Ok(text)` — post-delimiter text used as-is; the editor never opens.
/// * `Err(n)` for `n > 0` — a bare delimiter of `n` dots: the body editor
///   opens at the end of the flow, seeded with the `n`th template (1-based)
///   from `templates` (config `[editor] *_template` per task kind).
/// * `Err(0)` — no delimiter: no body.
/// * `Err(n)` for `n > 0` — a bare delimiter of `n` dots: the body editor
///   opens at the end of the flow, seeded with the `n`th template (1-based)
///   from `templates` (config `[editor] *_template` per task kind).
/// * `Err(0)` — no delimiter: no body.
fn resolve_body(body: Result<String, usize>, templates: &[PathBuf]) -> Result<String> {
    match body {
        Ok(b) => Ok(b),
        Err(0) => Ok(String::new()),
        Err(dots) => open_editor_for_body(templates, dots),
    }
}

pub(super) async fn create_task_command(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    task: Task,
) -> Result<()> {
    let task_type = task.task_type;
    let name = task.name;
    let body = task.body;
    let date = task.date;
    let prefill = task.prefill;
    let parent = task.parent;
    let pick_parent = task.pick_parent;
    let available_duration = task.available_duration;

    match task_type {
        TaskKind::Oneshot => {
            // Only a missing name triggers the interactive creation flow:
            // cliclack intro, then the name (required, unique, no tabs),
            // priority and target count. A body delimiter (`.`) is always
            // legal and resolves the same in every flow (see
            // [`resolve_body`]): `delimiter text` skips the body editor at
            // the end, a bare delimiter opens it, and no delimiter means
            // no body.
            let interactive = name.is_none();

            let (name_str, priority_val, target_count, parent_id) = if interactive {
                if !atty::is(atty::Stream::Stdin) {
                    anyhow::bail!("Oneshot task creation requires an interactive terminal");
                }

                crate::output::task_intro("Create oneshot task")?;

                // (Optional) Parent id: prompted only when no `-<parent_id>`
                // flag was given; blank input means no parent. The short id
                // is resolved to a row id here, so an unknown id fails
                // before anything is created. Before accepting a typed id
                // the parent's task name is confirmed — a "no" re-prompts
                // for the parent id.
                let parent_id = if let Some(short_id) = parent {
                    resolve_parent(pool, short_id).await?
                } else if pick_parent {
                    // A bare `-`: pick the parent in the oneshot picker
                    // TUI. The pick returns the row id directly; a
                    // cancelled pick means no parent.
                    crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                        .await?
                        .run()
                        .await?
                        .map(|(id, _)| id)
                } else {
                    loop {
                        match crate::prompts::prompt_parent_id()? {
                            None => break None,
                            Some(short_id) => {
                                let (id, name) = resolve_parent_named(pool, short_id).await?;
                                if crate::prompts::prompt_attach_parent(&name)? {
                                    break Some(id);
                                }
                                // Declined — loop and re-prompt the id.
                            }
                        }
                    }
                };

                // Name (required, unique among oneshot tasks, no tabs):
                // re-prompt on duplicates instead of aborting the flow.
                let name_str = prompt_unique_name(pool, None, Some(TaskKind::Oneshot)).await?;

                let priority_val = crate::prompts::prompt_priority(config.tasks.default_priority)?;
                let target_count = crate::prompts::prompt_target_count()?;

                (name_str, priority_val, target_count, parent_id)
            } else {
                // Command-line name: no prompts, default priority, single
                // completion (target_count = 0).
                let name_str = name.expect("a non-interactive oneshot task has a name");
                let parent_id = match parent {
                    Some(short_id) => resolve_parent(pool, short_id).await?,
                    None => {
                        if pick_parent {
                            anyhow::bail!(
                                "A bare '-' parent picker requires interactive creation (no task name)"
                            );
                        }
                        None
                    }
                };
                (name_str, config.tasks.default_priority, 0, parent_id)
            };

            // Name validity (non-empty, no tabs) before the task is
            // created. The interactive prompt enforces the same rules; the
            // command-line path needs them checked here.
            validate_name(&name_str)?;

            // Uniqueness for command-line names: the interactive flow
            // re-prompts on duplicates, so only this path bails.
            if !interactive
                && crate::db::task_name_exists(pool, &name_str, Some(TaskKind::Oneshot)).await?
            {
                anyhow::bail!("A task with name '{name_str}' already exists");
            }

            // Body: `delimiter text` is used as-is; a bare delimiter
            // opens the editor; no delimiter means no body. Same rules in
            // both flows.
            let body = resolve_body(body, &config.editor.task_template)?;

            // `@<time>` is the due time and lands in `end_time`; `start_time`
            // records the creation moment. The CLI parser already resolved
            // the due time to an epoch (`DATE_DIALECT`), so a bad time
            // fails before anything is created.
            let start_epoch = Some(crate::date::now());
            let end_epoch = date;

            // Both the stable row id and the user-facing short id are
            // assigned by the database layer (see sql.rs).
            let mut task_obj = TaskObject {
                id: None,
                short_id: None,
                name: name_str,
                body,
                priority: priority_val,
                start_time: start_epoch,
                available_duration_secs: None,
                interval_secs: None,
                target_count,
                optional: false,
                end_time: end_epoch,
                parent: parent_id,
            };
            let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
            task_obj.id = Some(new_id);
            task_obj.short_id = Some(new_short_id);

            if !opts.quiet() {
                println!(
                    "Created task #{}: {}",
                    task_obj.short_id.unwrap_or_default(),
                    task_obj.name
                );
                if opts.verbose() {
                    crate::output::print_rows(&crate::output::task_rows(&task_obj));
                }
            }
        }
        TaskKind::Recurring => {
            // Create new recurring task via interactive flow, with an
            // optional pre-filled name from `im ! @ <name>` and an optional
            // body from the body delimiter (editor only when the delimiter
            // is bare — no delimiter means no body).
            create_recurring_task(pool, config, opts, prefill, body).await?;
        }
        TaskKind::Scheduled => {
            // Scheduled task creation: `! @<time> [:name] [%<duration>]`.
            // The start time and duration were resolved to epochs at CLI
            // parse time, so bad values fail before any interactive prompt.
            // Creation happens immediately only when the start time, name
            // and duration all came from the command line; otherwise the
            // flow goes interactive with whatever was given pre-filled (a
            // pre-filled value skips its prompt).
            let start_epoch = date;
            let duration_secs = available_duration;

            if let (Some(name_str), Some(start), Some(dur)) =
                (name.as_deref(), start_epoch, duration_secs)
            {
                if name_str.contains('\t') {
                    anyhow::bail!("Task name cannot contain tab characters");
                }
                let body = resolve_body(body, &config.editor.scheduled_template)?;
                let mut task_obj = TaskObject {
                    id: None,
                    short_id: None,
                    name: name_str.to_string(),
                    body,
                    priority: config.tasks.default_scheduled_priority,
                    start_time: Some(start),
                    available_duration_secs: Some(dur),
                    interval_secs: None,
                    target_count: 0,
                    optional: false,
                    end_time: None,
                    parent: None,
                };
                let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
                task_obj.id = Some(new_id);
                task_obj.short_id = Some(new_short_id);

                if !opts.quiet() {
                    println!(
                        "Created task #{}: {}",
                        task_obj.short_id.unwrap_or_default(),
                        task_obj.name
                    );
                    if opts.verbose() {
                        crate::output::print_rows(&crate::output::task_rows(&task_obj));
                    }
                }
            } else {
                create_scheduled_task(pool, config, opts, name, start_epoch, duration_secs, body)
                    .await?;
            }
        }
    }

    Ok(())
}

async fn create_recurring_task(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    prefill: Option<String>,
    body: Result<String, usize>,
) -> Result<()> {
    use crate::date::{parse_duration_secs, parse_span};

    if !atty::is(atty::Stream::Stdin) {
        anyhow::bail!("Recurring task creation requires an interactive terminal");
    }

    crate::output::task_intro("Create recurring task")?;

    // 1. Task name (required, unique, no tabs) — re-prompt on duplicates
    // instead of aborting the whole flow. A pre-fill from `im ! @
    // <name>` skips the prompt entirely; on a duplicate the prompt
    // re-opens with the pre-fill as the default input so the user can
    // change it. The name is trimmed before use. The pre-filled value is
    // logged so the log file records what skipped the prompt.
    if let Some(p) = &prefill {
        cliclack::log::info(format!("Name: {p}"))?;
    }
    let name = prompt_unique_name(pool, prefill.as_deref(), Some(TaskKind::Recurring)).await?;

    // 2. Priority (1..=999 per validation; blank falls back to default).
    let priority = crate::prompts::prompt_priority(config.tasks.default_recurring_priority)?;

    // 3. Start time (blank = the current moment, `date::now()`). This is the
    // recurrence anchor: interval boundaries are computed from it
    // (`task::current_interval_start`), and the placeholder shows the
    // formatted default so the current anchor is visible before editing.
    let start_time = crate::prompts::prompt_start_time(None)?;

    // 4. Interval (required, valid duration; calendar-aware)
    let interval_str = crate::prompts::prompt_interval(None)?;
    let interval_span = parse_span(&interval_str)?;

    // 5. Available duration (blank = always available; capped at the
    // interval — availability beyond it means always available).
    let interval_rough_secs = crate::date::span_rough_seconds(interval_span) as i64;
    let avail_str =
        crate::prompts::prompt_available_duration(&interval_str, None, Some(interval_rough_secs))?;

    let available_duration_secs = if avail_str.is_empty() {
        None
    } else {
        let dur = parse_duration_secs(&avail_str)?;
        if dur >= interval_rough_secs {
            None
        } else {
            Some(dur)
        }
    };

    // 6. Target count (blank = 0, task can be completed once)
    let target_count = crate::prompts::prompt_target_count()?;

    // 7. End time (blank = never ends). `prompt_end` accepts a duration
    // (relative to now) or an absolute date/time and returns the epoch.
    let end_time = crate::prompts::prompt_end(None)?;

    // 8. Optional
    let is_optional = crate::prompts::prompt_optional(false)?;

    // 9. Body: `delimiter text` pre-fills the body (no editor); a bare
    // delimiter opens the body editor; no delimiter → no body.
    let body = resolve_body(body, &config.editor.recurring_template)?;

    // Insert into database. start_time marks the recurrence start (used as the
    // anchor for interval boundaries when applying completion deltas). Both the
    // stable row id and the user-facing short id are assigned by the database
    // layer (see sql.rs).
    let mut task_obj = TaskObject {
        id: None,
        short_id: None,
        name,
        body,
        priority,
        start_time: Some(start_time),
        available_duration_secs,
        interval_secs: Some(crate::date::span_to_db(&interval_span)),
        target_count,
        optional: is_optional,
        end_time,
        parent: None,
    };
    let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
    task_obj.id = Some(new_id);
    task_obj.short_id = Some(new_short_id);

    if !opts.quiet() {
        println!(
            "Created task #{}: {}",
            task_obj.short_id.unwrap_or_default(),
            task_obj.name
        );
        if opts.verbose() {
            crate::output::print_rows(&crate::output::task_rows(&task_obj));
        }
    }

    Ok(())
}

/// Interactive scheduled creation flow (`! @<time> [:name] [%<duration>]`
/// with anything missing from the command line). Mirrors the recurring flow:
/// required name (unique, re-prompt on duplicates) and start time, then the
/// available duration (blank → 1 hour), then priority. Scheduled tasks always
/// have target_count 0, so there is no target prompt. Values that came from
/// the command line skip their prompt.
async fn create_scheduled_task(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    name: Option<String>,
    start: Option<i64>,
    duration: Option<i64>,
    body: Result<String, usize>,
) -> Result<()> {
    use crate::date::parse_duration_secs;

    if !atty::is(atty::Stream::Stdin) {
        anyhow::bail!("Scheduled task creation requires an interactive terminal");
    }

    crate::output::task_intro("Create scheduled task")?;

    // 1. Task name (required, unique, no tabs). A name from the command
    // line skips the prompt entirely; on a duplicate the prompt re-opens
    // with the given name as the default input so the user can change it.
    if let Some(n) = &name {
        cliclack::log::info(format!("Name: {n}"))?;
    }
    let name = prompt_unique_name(pool, name.as_deref(), Some(TaskKind::Scheduled)).await?;

    // 2. Start time (required). A start time from the command line skips
    // the prompt; blank in the prompt means "now".
    let start = match start {
        Some(s) => {
            cliclack::log::info(format!("Start: {}", crate::date::format_datetime(s)))?;
            s
        }
        None => crate::prompts::prompt_start_time(None)?,
    };

    // 3. Available duration (required for scheduled tasks). A duration from
    // the command line (parsed to seconds in the caller) skips the prompt;
    // blank means the 1-hour default.
    let duration_secs = match duration {
        Some(d) => {
            cliclack::log::info(format!("Duration: {}", format_duration(d)))?;
            d
        }
        None => {
            let raw = crate::prompts::prompt_available_duration("1 hour", None, None)?;
            if raw.trim().is_empty() {
                3600
            } else {
                parse_duration_secs(&raw)?
            }
        }
    };

    // 4. Priority (blank falls back to the scheduled default).
    let priority = crate::prompts::prompt_priority(config.tasks.default_scheduled_priority)?;

    // 5. Body: `delimiter text` pre-fills the body (no editor); a bare
    // delimiter opens the body editor; no delimiter → no body.
    let body = resolve_body(body, &config.editor.scheduled_template)?;

    let mut task_obj = TaskObject {
        id: None,
        short_id: None,
        name,
        body,
        priority,
        start_time: Some(start),
        available_duration_secs: Some(duration_secs),
        interval_secs: None,
        target_count: 0,
        optional: false,
        end_time: None,
        parent: None,
    };
    let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
    task_obj.id = Some(new_id);
    task_obj.short_id = Some(new_short_id);

    if !opts.quiet() {
        println!(
            "Created task #{}: {}",
            task_obj.short_id.unwrap_or_default(),
            task_obj.name
        );
        if opts.verbose() {
            crate::output::print_rows(&crate::output::task_rows(&task_obj));
        }
    }

    Ok(())
}

/// Validate a task name: non-empty after trimming and free of tab
/// characters (view output uses tab separators). Called on the resolved
/// name before any task is created; the interactive prompt enforces the
/// same rules via its cliclack `validate` closure.
fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("Task name is required");
    }
    if name.contains('\t') {
        anyhow::bail!("Task name cannot contain tab characters");
    }
    Ok(())
}

/// Resolve a unique, non-empty task name for creation. A name from the
/// command line skips the prompt entirely; on a duplicate the prompt
/// re-opens with the given name as the default input so the user can
/// change it. `task_type` scopes the uniqueness check to that kind (see
/// [`crate::db::task_name_exists`]).
async fn prompt_unique_name(
    pool: &sqlx::SqlitePool,
    given: Option<&str>,
    task_type: Option<TaskKind>,
) -> Result<String> {
    let given = given.map(str::trim).filter(|s| !s.is_empty());
    if let Some(name) = given
        && !crate::db::task_name_exists(pool, name, task_type).await? {
            return Ok(name.to_string());
        }
    loop {
        let candidate = crate::prompts::prompt_name(given)?;
        if crate::db::task_name_exists(pool, &candidate, task_type).await? {
            cliclack::log::error(format!("A task with name '{candidate}' already exists"))?;
            continue;
        }
        return Ok(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_body, validate_name};

    /// `validate_name` accepts a normal name and rejects empty and
    /// tab-containing ones (the rules enforced before task creation).
    #[test]
    fn validate_name_rules() {
        assert!(validate_name("exercise").is_ok());
        assert!(
            validate_name("  padded  ").is_ok(),
            "surrounding space is trimmed"
        );

        assert!(validate_name("").is_err(), "empty name must be rejected");
        assert!(
            validate_name("   ").is_err(),
            "whitespace-only name must be rejected"
        );
        assert!(
            validate_name("a\tb").is_err(),
            "embedded tab must be rejected"
        );
        assert!(
            validate_name("\ttab").is_err(),
            "leading tab must be rejected"
        );
        assert!(
            validate_name("tab\t").is_err(),
            "trailing tab must be rejected"
        );
    }

    /// `resolve_body` passes `Ok(text)` through as-is and maps `Err(0)`
    /// (no delimiter) to an empty body without opening the editor. The
    /// bare-delimiter path (`Err(n)`) routes to the editor, exercised
    /// end-to-end in editor.rs via the fake-editor tests.
    #[test]
    fn resolve_body_editor_only_on_bare_delimiter() {
        assert_eq!(
            resolve_body(Ok("notes".to_string()), &[]).unwrap(),
            "notes",
            "post-delimiter text is used as-is"
        );
        assert_eq!(
            resolve_body(Err(0), &[]).unwrap(),
            "",
            "no delimiter means no body and no editor"
        );
    }
}
