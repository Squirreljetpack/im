//! Interactive creation flows launched from the TUI (`ctrl-n`): pick a
//! target, run its prompts, and write through the same paths as the CLI.
//! Returns a one-line outcome for the caller's toast (`None` when the user
//! cancelled or nothing was written).

use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::Config;
use crate::prompts::CreateKind;
use crate::types::{Entry, Task, TaskKind};

pub(crate) async fn create_interactive(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    include_entries: bool,
) -> Result<Option<String>> {
    match crate::prompts::prompt_create_kind(include_entries)? {
        CreateKind::Mood => create_mood(pool, config, opts).await,
        CreateKind::TrackerEntry => {
            let logged = crate::ui::tracker_picker::run_trackers_app(config).await?;
            Ok(logged.then(|| "Tracker entry logged".to_string()))
        }
        CreateKind::Session => create_session(pool, config, opts).await,
        CreateKind::Oneshot => create_task(pool, config, opts, TaskKind::Oneshot).await,
        CreateKind::Recurring => create_task(pool, config, opts, TaskKind::Recurring).await,
        CreateKind::Scheduled => create_task(pool, config, opts, TaskKind::Scheduled).await,
    }
}

/// Mood and body are both optional; two empty answers write nothing.
async fn create_mood(pool: &SqlitePool, config: &Config, opts: &CliOpts) -> Result<Option<String>> {
    let mood = crate::prompts::prompt_mood()?;
    let body = crate::prompts::prompt_body()?;
    if mood.is_empty() && body.is_empty() {
        return Ok(None);
    }
    let wrote = super::entry::record_entry(
        pool,
        config,
        opts,
        Entry {
            mood,
            trackers: Vec::new(),
            task_ref: None,
            count: None,
            body: Ok(body),
            duration: None,
        },
    )
    .await?;
    Ok(wrote.then(|| "Entry logged".to_string()))
}

async fn create_session(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
) -> Result<Option<String>> {
    let secs = crate::prompts::prompt_session_duration()?;
    let wrote = super::entry::record_entry(
        pool,
        config,
        opts,
        Entry {
            mood: String::new(),
            trackers: Vec::new(),
            task_ref: None,
            count: None,
            body: Err(0),
            duration: Some(secs),
        },
    )
    .await?;
    Ok(wrote.then(|| "Session logged".to_string()))
}

async fn create_task(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    task_type: TaskKind,
) -> Result<Option<String>> {
    let task = Task {
        task_type,
        name: None,
        priority: None,
        date: None,
        body: Err(0),
        prefill: None,
        parent: None,
        available_duration: None,
        interval: None,
    };
    super::task::create_task_command(pool, config, opts, task).await
}
