use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::Config;
use crate::db::Id;
use crate::types::{TaskKind, TaskRef};

pub(super) async fn handle_task_edit(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    task_ref: Option<TaskRef>,
) -> Result<()> {
    if !atty::is(atty::Stream::Stdin) {
        anyhow::bail!("Task editing requires an interactive terminal");
    }

    // Resolve task id
    let task_id = match task_ref {
        None | Some(TaskRef::Pick) => {
            let picked =
                crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                    .await?
                    .run()
                    .await?;
            let Some((id, _)) = picked else {
                return Ok(());
            };
            id
        }
        Some(task_ref) => super::refs::resolve_task_ref(pool, &task_ref).await?,
    };

    let task = crate::db::fetch_task_by_id(pool, task_id, crate::date::now())
        .await?
        .ok_or_else(|| anyhow::anyhow!("Task not found"))?;

    let mut name = task.name.clone();
    let mut body = task.body.clone();
    let mut priority = task.priority;
    let mut short_id = task.short_id;
    let mut parent = task.parent;
    let mut start_time = task.start_time;
    let mut end_time = task.end_time;
    let mut available_duration_secs = task.available_duration_secs;
    let mut interval_secs = task.interval_secs;
    let mut target_count = task.target_count;
    let mut optional = task.optional != 0;

    crate::output::task_intro("Edit task")?;

    loop {
        let parent_label = if let Some(pid) = parent {
            if let Ok(Some(pt)) = crate::db::fetch_task_by_id(pool, pid, crate::date::now()).await {
                pt.name
            } else {
                "unknown".to_string()
            }
        } else {
            "none".to_string()
        };

        let body_hint = if body.is_empty() {
            "empty".to_string()
        } else {
            let first_line = body.lines().next().unwrap_or("");
            if first_line.chars().count() > 20 {
                let s: String = first_line.chars().take(17).collect();
                format!("{}...", s)
            } else {
                first_line.to_string()
            }
        };

        let mut select = cliclack::select("Edit field:");
        select = select.item("name", "Name", &name);
        select = select.item(
            "short_id",
            "Short ID",
            short_id
                .map(|s| s.to_string())
                .unwrap_or_else(|| "none".to_string()),
        );
        select = select.item("priority", "Priority", priority.to_string());
        select = select.item("body", "Body", &body_hint);
        select = select.item("parent", "Parent", &parent_label);

        match task.kind() {
            TaskKind::Oneshot => {
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
            }
            TaskKind::Recurring => {
                select = select.item(
                    "start",
                    "Start time",
                    start_time
                        .map(crate::date::format_datetime)
                        .unwrap_or_else(|| "now".to_string()),
                );
                select = select.item(
                    "interval",
                    "Interval",
                    interval_secs
                        .map(|s| crate::date::format_span(&crate::date::db_to_span(s)))
                        .unwrap_or_else(|| "none".to_string()),
                );
                select = select.item(
                    "available_duration",
                    "Available duration",
                    available_duration_secs
                        .map(crate::date::format_duration)
                        .unwrap_or_else(|| "always".to_string()),
                );
                select = select.item(
                    "target",
                    "Times to complete per interval",
                    &if target_count == 0 {
                        "once".to_string()
                    } else {
                        target_count.to_string()
                    },
                );
                select = select.item("optional", "Optional", if optional { "Yes" } else { "No" });
                select = select.item(
                    "end",
                    "End time",
                    end_time
                        .map(|ts| crate::date::format_human_datetime(ts, true))
                        .unwrap_or_else(|| "never".to_string()),
                );
            }
            TaskKind::Scheduled => {
                select = select.item(
                    "start",
                    "Start time",
                    start_time
                        .map(crate::date::format_datetime)
                        .unwrap_or_else(|| "now".to_string()),
                );
                select = select.item(
                    "available_duration",
                    "Available duration",
                    available_duration_secs
                        .map(crate::date::format_duration)
                        .unwrap_or_else(|| "1 hour".to_string()),
                );
                select = select.item("optional", "Optional", if optional { "Yes" } else { "No" });
            }
        }

        select = select.item("save", "Save", "");
        select = select.item("cancel", "Cancel", "");

        let action = select.interact()?;

        match action {
            "name" => {
                if let Ok(n) = crate::prompts::prompt_name(Some(&name)) {
                    name = n;
                }
            }
            "short_id" => {
                if let Ok(s) = crate::prompts::prompt_short_id(pool, task.id, short_id).await {
                    short_id = s;
                }
            }
            "priority" => {
                if let Ok(p) = crate::prompts::prompt_priority(priority) {
                    priority = p;
                }
            }
            "body" => {
                if let Ok(b) = crate::editor::open_editor_on_text(&body) {
                    body = b;
                }
            }
            "parent" => {
                let picked =
                    crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                        .await?
                        .run()
                        .await?;
                if let Some((id, _)) = picked {
                    parent = Some(id);
                }
            }
            "due" => {
                let cur = end_time.map(crate::date::format_datetime);
                if let Ok(t) = crate::prompts::prompt_start_time("Due time:", cur.as_deref()) {
                    end_time = Some(t);
                }
            }
            "start" => {
                let cur = start_time.map(crate::date::format_datetime);
                if let Ok(t) = crate::prompts::prompt_start_time("Start time:", cur.as_deref()) {
                    start_time = Some(t);
                }
            }
            "interval" => {
                let cur =
                    interval_secs.map(|s| crate::date::format_span(&crate::date::db_to_span(s)));
                if let Ok(s) = crate::prompts::prompt_interval(cur.as_deref()) {
                    let span = crate::date::parse_span(&s)?;
                    let packed = crate::date::span_to_db(&span);
                    interval_secs = Some(packed);
                    // A duration that no longer fits the new interval means
                    // always available.
                    available_duration_secs = crate::task::availability_within_interval(
                        available_duration_secs,
                        crate::date::db_span_rough_secs(packed),
                    );
                }
            }
            "available_duration" => {
                let (interval_label, rough_secs) = if let Some(iv) = interval_secs {
                    (
                        crate::date::format_span(&crate::date::db_to_span(iv)),
                        Some(crate::date::db_span_rough_secs(iv)),
                    )
                } else {
                    ("1 hour".to_string(), None)
                };
                let cur = available_duration_secs.map(crate::date::format_duration);
                if let Ok(dur) = crate::prompts::prompt_available_duration(
                    &interval_label,
                    cur.as_deref(),
                    rough_secs,
                ) {
                    // Blank means "always" for a recurring task, whose
                    // availability can be unbounded. A scheduled window is
                    // mandatory, so a blank answer keeps the current duration.
                    if rough_secs.is_some() || dur.is_some() {
                        available_duration_secs = dur;
                    }
                }
            }
            "target" => {
                if let Ok(n) = crate::prompts::prompt_target_count() {
                    target_count = n;
                }
            }
            "optional" => {
                if let Ok(o) = crate::prompts::prompt_optional(optional) {
                    optional = o;
                }
            }
            "end" => {
                let cur = end_time.map(crate::date::format_datetime);
                if let Ok(e) = crate::prompts::prompt_end(cur.as_deref()) {
                    end_time = e;
                }
            }
            "save" => {
                if let Err(e) = validate_edit_name(pool, task.id, &name, task.kind()).await {
                    cliclack::log::error(format!("{e}"))?;
                    continue;
                }
                let update_obj = crate::db::UpdateTaskObject {
                    id: task.id,
                    short_id,
                    name: name.clone(),
                    body: body.clone(),
                    priority,
                    start_time,
                    available_duration_secs,
                    interval_secs,
                    target_count,
                    optional,
                    end_time,
                    parent,
                };
                crate::db::edit_task(pool, &update_obj).await?;
                if !opts.quiet() {
                    let msg = match short_id {
                        Some(id) => format!("Updated task #{id}: {name}"),
                        None => format!("Updated task: {name}"),
                    };
                    cliclack::outro(msg)?;
                }
                break;
            }
            "cancel" => {
                cliclack::outro_cancel("Cancelled.")?;
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Re-validate the edited name before saving: non-empty, no tabs, and
/// unique among the task's kind excluding the task itself (creation
/// enforces per-kind uniqueness; the edit must not introduce a
/// duplicate). Returns `Err` so the caller stays in the menu.
async fn validate_edit_name(
    pool: &SqlitePool,
    current_id: Id,
    name: &str,
    kind: TaskKind,
) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("Task name is required");
    }
    if name.contains('\t') {
        anyhow::bail!("Task name cannot contain tab characters");
    }
    if crate::db::task_name_exists(pool, name, Some(kind), Some(current_id)).await? {
        anyhow::bail!("A task with name '{name}' already exists");
    }
    Ok(())
}
