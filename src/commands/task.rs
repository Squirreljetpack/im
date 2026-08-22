use std::path::PathBuf;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::Config;
use crate::date::format_duration;
use crate::db::TaskObject;
use crate::editor::open_editor_for_body;
use crate::types::{Task, TaskKind, TaskRef};

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

/// Resolve a TaskRef to its stable row id plus the task name.
pub(crate) async fn resolve_task_ref_named(
    pool: &SqlitePool,
    task_ref: &TaskRef,
) -> Result<(i64, String)> {
    match task_ref {
        TaskRef::Id(short_id) => resolve_parent_named(pool, *short_id).await,
        TaskRef::Words(words) => {
            let matches = crate::db::fetch_task_matching_words(pool, words).await?;
            match matches.len() {
                0 => anyhow::bail!("No task found matching query '{}'", words.join(" ")),
                1 => Ok((matches[0].id, matches[0].name.clone())),
                n => anyhow::bail!("Multiple tasks match query '{}' (found {})", words.join(" "), n),
            }
        }
        TaskRef::Pick => {
            anyhow::bail!("Cannot resolve Pick task_ref without interactive TUI");
        }
    }
}

/// Resolve a TaskRef to a stable row id.
pub(crate) async fn resolve_parent(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    parent_ref: &TaskRef,
) -> Result<Option<i64>> {
    match parent_ref {
        TaskRef::Id(short_id) => Ok(Some(resolve_parent_named(pool, *short_id).await?.0)),
        TaskRef::Words(words) => {
            let matches = crate::db::fetch_task_matching_words(pool, words).await?;
            match matches.len() {
                0 => anyhow::bail!("No task found matching query '{}'", words.join(" ")),
                1 => Ok(Some(matches[0].id)),
                n => anyhow::bail!("Multiple tasks match query '{}' (found {})", words.join(" "), n),
            }
        }
        TaskRef::Pick => {
            // Cancelling the picker cancels the whole operation.
            let picked = crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                .await?
                .run()
                .await?;
            match picked {
                Some((id, _)) => Ok(Some(id)),
                None => anyhow::bail!("Cancelled"),
            }
        }
    }
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
    let available_duration = task.available_duration;
    let pick_duration = task.pick_duration;

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

            let (name_str, body_str, priority_val, target_count, optional_val, parent_id, end_epoch) = if interactive {
                if !atty::is(atty::Stream::Stdin) {
                    anyhow::bail!("Oneshot task creation requires an interactive terminal");
                }

                crate::output::task_intro("Create oneshot task")?;

                // Name (required, unique among oneshot tasks, no tabs):
                // prompted before entering the edit menu.
                let name_val = prompt_unique_name(pool, None, Some(TaskKind::Oneshot)).await?;
                let mut body_val = resolve_body(body, &config.editor.task_template)?;
                let mut priority_val = config.tasks.default_priority;
                let mut parent_id: Option<i64> = if let Some(ref p_ref) = parent {
                    resolve_parent(pool, config, opts, p_ref).await?
                } else {
                    None
                };
                let mut end_time: Option<i64> = date;
                let mut target_count: i32 = 0;
                let mut optional: bool = false;

                loop {
                    let parent_label = if let Some(pid) = parent_id {
                        if let Ok(Some(pt)) = crate::db::fetch_task_by_id(pool, pid, crate::date::now()).await {
                            pt.name
                        } else {
                            "unknown".to_string()
                        }
                    } else {
                        "none".to_string()
                    };

                    let body_hint = if body_val.is_empty() {
                        "empty".to_string()
                    } else {
                        let first_line = body_val.lines().next().unwrap_or("");
                        if first_line.chars().count() > 20 {
                            let s: String = first_line.chars().take(17).collect();
                            format!("{}...", s)
                        } else {
                            first_line.to_string()
                        }
                    };

                    let mut select = cliclack::select("Edit fields:");
                    select = select.item("save", "Save", "");
                    select = select.item("priority", "Priority", priority_val.to_string());
                    select = select.item("body", "Body", &body_hint);
                    select = select.item("parent", "Parent", &parent_label);
                    select = select.item(
                        "due",
                        "Due",
                        end_time
                            .map(|ts| crate::date::format_human_datetime(ts, true))
                            .unwrap_or_else(|| "none".to_string()),
                    );
                    select = select.item(
                        "target",
                        "Times to complete",
                        if target_count == 0 {
                            "once".to_string()
                        } else {
                            target_count.to_string()
                        },
                    );
                    select = select.item("optional", "Optional", if optional { "Yes" } else { "No" });
                    select = select.item("cancel", "Cancel", "");

                    let action = select.interact()?;

                    match action {
                        "priority" => {
                            if let Ok(p) = crate::prompts::prompt_priority(priority_val) {
                                priority_val = p;
                            }
                        }
                        "body" => {
                            if let Ok(b) = crate::editor::open_editor_on_text(&body_val) {
                                body_val = b;
                            }
                        }
                        "parent" => {
                            let picked = crate::ui::oneshots::OneshotPickerApp::new(
                                config.clone(),
                                opts.fullscreen,
                            )
                            .await?
                            .run()
                            .await?;
                            if let Some((id, _)) = picked {
                                parent_id = Some(id);
                            }
                        }
                        "due" => {
                            let cur = end_time.map(crate::date::format_datetime);
                            if let Ok(t) = crate::prompts::prompt_start_time("Due time:", cur.as_deref()) {
                                end_time = Some(t);
                            }
                        }
                        "target" => {
                            if let Ok(tc) = crate::prompts::prompt_target_count() {
                                target_count = tc;
                            }
                        }
                        "optional" => {
                            if let Ok(opt) = crate::prompts::prompt_optional(optional) {
                                optional = opt;
                            }
                        }
                        "save" => break,
                        "cancel" => {
                            cliclack::outro_cancel("Cancelled.")?;
                            return Ok(());
                        }
                        _ => unreachable!(),
                    }
                }

                (name_val, body_val, priority_val, target_count, optional, parent_id, end_time)
            } else {
                // Command-line name: no prompts, default priority, single
                // completion (target_count = 0).
                let name_str = name.expect("a non-interactive oneshot task has a name");
                let parent_id = match parent {
                    Some(ref p_ref) => {
                        if matches!(p_ref, TaskRef::Pick) {
                            anyhow::bail!(
                                "A bare '+' parent picker requires interactive creation (no task name)"
                            );
                        }
                        resolve_parent(pool, config, opts, p_ref).await?
                    }
                    None => None,
                };
                let body_str = resolve_body(body, &config.editor.task_template)?;
                (name_str, body_str, config.tasks.default_priority, 0, false, parent_id, date)
            };

            // Name validity (non-empty, no tabs) before the task is
            // created. The interactive prompt enforces the same rules; the
            // command-line path needs them checked here.
            validate_name(&name_str)?;

            // Uniqueness for command-line names: the interactive flow
            // re-prompts on duplicates, so only this path bails.
            if !interactive
                && crate::db::task_name_exists(pool, &name_str, Some(TaskKind::Oneshot), None).await?
            {
                anyhow::bail!("A task with name '{name_str}' already exists");
            }

            // `@<time>` is the due time and lands in `end_time`; `start_time`
            // records the creation moment.
            let start_epoch = Some(crate::date::now());

            // Both the stable row id and the user-facing short id are
            // assigned by the database layer (see sql.rs).
            let mut task_obj = TaskObject {
                id: None,
                short_id: None,
                name: name_str,
                body: body_str,
                priority: priority_val,
                start_time: start_epoch,
                available_duration_secs: None,
                interval_secs: None,
                target_count,
                optional: optional_val,
                end_time: end_epoch,
                parent: parent_id,
            };
            let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
            task_obj.id = Some(new_id);
            task_obj.short_id = Some(new_short_id);

            if !opts.quiet() {
                if interactive {
                    cliclack::outro(format!(
                        "Created task #{}: {}",
                        task_obj.short_id.unwrap_or_default(),
                        task_obj.name
                    ))?;
                } else {
                    println!(
                        "Created task #{}: {}",
                        task_obj.short_id.unwrap_or_default(),
                        task_obj.name
                    );
                }
                if opts.verbose() {
                    crate::output::print_rows(&crate::output::task_rows(&task_obj));
                }
            }
        }
        TaskKind::Recurring => {
            // Create new recurring task via interactive flow, with an
            // optional pre-filled name from `im ! %<duration> <name>` and an optional
            // body from the body delimiter (editor only when the delimiter
            // is bare — no delimiter means no body).
            create_recurring_task(
                pool,
                config,
                opts,
                prefill,
                body,
                available_duration,
                pick_duration,
            )
            .await?;
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
    _opts: &CliOpts,
    prefill: Option<String>,
    body: Result<String, usize>,
    cl_duration: Option<i64>,
    _pick_duration: bool,
) -> Result<()> {
    use crate::date::{parse_duration_secs, parse_span};

    if !atty::is(atty::Stream::Stdin) {
        anyhow::bail!("Recurring task creation requires an interactive terminal");
    }

    crate::output::task_intro("Create recurring task")?;

    // 1. Task name (prompted before menu)
    if let Some(p) = &prefill {
        cliclack::log::info(format!("Name: {p}"))?;
    }
    let name = prompt_unique_name(pool, prefill.as_deref(), Some(TaskKind::Recurring)).await?;

    // 2. Start time (prompted before menu)
    let start_time = crate::prompts::prompt_start_time("Start time:", None)?;

    // 3. Duration / interval (prompted before menu)
    let interval_str = crate::prompts::prompt_interval(None)?;
    let interval_span = parse_span(&interval_str)?;
    let interval_rough_secs = crate::date::span_rough_seconds(interval_span) as i64;

    // 4. Available duration (prompted before menu)
    let available_duration_secs: Option<i64> = if let Some(dur) = cl_duration {
        cliclack::log::info(format!("Available duration: {}", format_duration(dur)))?;
        if dur >= interval_rough_secs {
            None
        } else {
            Some(dur)
        }
    } else {
        let avail_str = crate::prompts::prompt_available_duration(
            &interval_str,
            None,
            Some(interval_rough_secs),
        )?;
        if avail_str.trim().is_empty() {
            None
        } else {
            let dur = parse_duration_secs(&avail_str)?;
            if dur >= interval_rough_secs {
                None
            } else {
                Some(dur)
            }
        }
    };

    // Remaining fields configured via edit menu
    let mut priority_val = config.tasks.default_recurring_priority;
    let mut body_val = resolve_body(body, &config.editor.recurring_template)?;
    let mut target_count: i32 = 0;
    let mut end_time: Option<i64> = None;
    let mut optional: bool = false;

    loop {
        let body_hint = if body_val.is_empty() {
            "empty".to_string()
        } else {
            let first_line = body_val.lines().next().unwrap_or("");
            if first_line.chars().count() > 20 {
                let s: String = first_line.chars().take(17).collect();
                format!("{}...", s)
            } else {
                first_line.to_string()
            }
        };

        let mut select = cliclack::select("Edit fields:");
        select = select.item("save", "Save", "");
        select = select.item("priority", "Priority", priority_val.to_string());
        select = select.item("body", "Body", &body_hint);
        select = select.item(
            "target",
            "Times to complete",
            if target_count == 0 {
                "once".to_string()
            } else {
                target_count.to_string()
            },
        );
        select = select.item(
            "end",
            "End",
            end_time
                .map(|ts| crate::date::format_human_datetime(ts, true))
                .unwrap_or_else(|| "none".to_string()),
        );
        select = select.item("optional", "Optional", if optional { "Yes" } else { "No" });
        select = select.item("cancel", "Cancel", "");

        let action = select.interact()?;

        match action {
            "priority" => {
                if let Ok(p) = crate::prompts::prompt_priority(priority_val) {
                    priority_val = p;
                }
            }
            "body" => {
                if let Ok(b) = crate::editor::open_editor_on_text(&body_val) {
                    body_val = b;
                }
            }
            "target" => {
                if let Ok(tc) = crate::prompts::prompt_target_count() {
                    target_count = tc;
                }
            }
            "end" => {
                let cur = end_time.map(crate::date::format_datetime);
                if let Ok(e) = crate::prompts::prompt_end(cur.as_deref()) {
                    end_time = e;
                }
            }
            "optional" => {
                if let Ok(opt) = crate::prompts::prompt_optional(optional) {
                    optional = opt;
                }
            }
            "save" => break,
            "cancel" => {
                cliclack::outro_cancel("Cancelled.")?;
                return Ok(());
            }
            _ => unreachable!(),
        }
    }

    let mut task_obj = TaskObject {
        id: None,
        short_id: None,
        name,
        body: body_val,
        priority: priority_val,
        start_time: Some(start_time),
        available_duration_secs,
        interval_secs: Some(crate::date::span_to_db(&interval_span)),
        target_count,
        optional,
        end_time,
        parent: None,
    };
    let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
    task_obj.id = Some(new_id);
    task_obj.short_id = Some(new_short_id);

    if !_opts.quiet() {
        cliclack::outro(format!(
            "Created task #{}: {}",
            task_obj.short_id.unwrap_or_default(),
            task_obj.name
        ))?;
        if _opts.verbose() {
            crate::output::print_rows(&crate::output::task_rows(&task_obj));
        }
    }

    Ok(())
}

/// Interactive scheduled creation flow (`! @<time> [:name] [%<duration>]`
/// with anything missing from the command line). Prompts name, start time,
/// and available duration upfront, then allows editing priority, body,
/// and optional in the menu before saving.
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

    // 1. Task name (prompted before menu)
    if let Some(n) = &name {
        cliclack::log::info(format!("Name: {n}"))?;
    }
    let name = prompt_unique_name(pool, name.as_deref(), Some(TaskKind::Scheduled)).await?;

    // 2. Start time (prompted before menu)
    let start = match start {
        Some(s) => {
            cliclack::log::info(format!("Start: {}", crate::date::format_datetime(s)))?;
            s
        }
        None => crate::prompts::prompt_start_time("Start time:", None)?,
    };

    // 3. Available duration (prompted before menu)
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

    // Remaining fields configured via edit menu
    let mut priority_val = config.tasks.default_scheduled_priority;
    let mut body_val = resolve_body(body, &config.editor.scheduled_template)?;
    let mut optional: bool = false;

    loop {
        let body_hint = if body_val.is_empty() {
            "empty".to_string()
        } else {
            let first_line = body_val.lines().next().unwrap_or("");
            if first_line.chars().count() > 20 {
                let s: String = first_line.chars().take(17).collect();
                format!("{}...", s)
            } else {
                first_line.to_string()
            }
        };

        let mut select = cliclack::select("Edit fields:");
        select = select.item("save", "Save", "");
        select = select.item("priority", "Priority", priority_val.to_string());
        select = select.item("body", "Body", &body_hint);
        select = select.item("optional", "Optional", if optional { "Yes" } else { "No" });
        select = select.item("cancel", "Cancel", "");

        let action = select.interact()?;

        match action {
            "priority" => {
                if let Ok(p) = crate::prompts::prompt_priority(priority_val) {
                    priority_val = p;
                }
            }
            "body" => {
                if let Ok(b) = crate::editor::open_editor_on_text(&body_val) {
                    body_val = b;
                }
            }
            "optional" => {
                if let Ok(opt) = crate::prompts::prompt_optional(optional) {
                    optional = opt;
                }
            }
            "save" => break,
            "cancel" => {
                cliclack::outro_cancel("Cancelled.")?;
                return Ok(());
            }
            _ => unreachable!(),
        }
    }

    let mut task_obj = TaskObject {
        id: None,
        short_id: None,
        name,
        body: body_val,
        priority: priority_val,
        start_time: Some(start),
        available_duration_secs: Some(duration_secs),
        interval_secs: None,
        target_count: 0,
        optional,
        end_time: None,
        parent: None,
    };
    let (new_id, new_short_id) = crate::db::create_task(pool, &task_obj).await?;
    task_obj.id = Some(new_id);
    task_obj.short_id = Some(new_short_id);

    if !opts.quiet() {
        cliclack::outro(format!(
            "Created task #{}: {}",
            task_obj.short_id.unwrap_or_default(),
            task_obj.name
        ))?;
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
        && !crate::db::task_name_exists(pool, name, task_type, None).await? {
            return Ok(name.to_string());
        }
    loop {
        let candidate = crate::prompts::prompt_name(given)?;
        if crate::db::task_name_exists(pool, &candidate, task_type, None).await? {
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
