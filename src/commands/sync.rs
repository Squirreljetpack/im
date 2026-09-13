//! `im sync` — push the queued events and apply the other devices' events.
//!
//! Every event is applied by field-level last-write-wins (§4.1); the one thing
//! the user decides here is a conflict between the incoming event and a change
//! this device has not pushed yet (§4.2).

use std::time::Duration;

use anyhow::{Result, bail};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::sync::apply::{Conflict, Resolution};
use crate::sync::client::server_url;
use crate::sync::session::{self, sync_once};
use crate::tracker::TrackerSlots;

/// How long `im sync` waits on the server: it is the interactive path, so a
/// slow connection is worth waiting for.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Safety net for the page loop: the server pages at 1000 events, so a
/// backlog deeper than this is a server problem, not a sync in progress.
const MAX_PAGES: usize = 10_000;

pub async fn sync_command(pool: &SqlitePool, config: &Config) -> Result<()> {
    let server = server_url();
    let slots = TrackerSlots::from_config(config);
    let interactive = atty::is(atty::Stream::Stdin);
    let mut pushed = 0;
    let mut applied = 0;
    let mut stale = 0;
    let mut conflicts = 0;

    for _ in 0..MAX_PAGES {
        let report = sync_once(pool, &server, TIMEOUT, &slots).await?;
        pushed += report.pushed;
        applied += report.applied;
        stale += report.stale;

        if !report.conflicts.is_empty() {
            if !interactive {
                bail!(
                    "{} event(s) conflict with this device's changes — run `im sync` in a terminal to \
                     settle them",
                    report.conflicts.len()
                );
            }
            conflicts += report.conflicts.len();
            for conflict in &report.conflicts {
                let resolution = prompt_resolution(pool, conflict).await?;
                session::resolve(pool, conflict, resolution).await?;
            }
            continue;
        }
        if report.has_more {
            continue;
        }

        println!("Pushed {pushed} event(s); applied {applied}; already known {stale}.");
        if conflicts == 0 {
            println!("Up to date at server version {}.", report.server_version);
        } else {
            println!(
                "Settled {conflicts} conflict(s); up to date at server version {}.",
                report.server_version
            );
        }
        return Ok(());
    }
    bail!("still pulling after {MAX_PAGES} pages; stopping")
}

/// Ask the user to choose one of the conflict's resolutions.
async fn prompt_resolution(pool: &SqlitePool, conflict: &Conflict) -> Result<Resolution> {
    let options = conflict.kind.options(conflict);
    let mut prompt = cliclack::select(conflict_question(pool, conflict).await);
    for (resolution, label, hint) in options {
        prompt = prompt.item(resolution, label, hint);
    }
    prompt
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))
}

/// The question line: what the conflict is about, with the values it competes
/// with so the choice is not blind.
async fn conflict_question(pool: &SqlitePool, conflict: &Conflict) -> String {
    let entity = entity_label(pool, conflict).await;
    match conflict.kind {
        crate::sync::ConflictKind::TextReplaced => format!(
            "{} — \"{}\" is \"{}\" here and \"{}\" elsewhere. {}",
            conflict.kind.describe(),
            field_label(conflict),
            conflict.local_value.as_deref().unwrap_or("(empty)"),
            conflict.remote_value.as_deref().unwrap_or("(empty)"),
            entity,
        ),
        crate::sync::ConflictKind::ParentCycle => format!(
            "{}: {} and {} ({})",
            conflict.kind.describe(),
            parent_label(pool, conflict.local_parent, "this device's parent").await,
            parent_label(pool, conflict.remote_parent, "the incoming parent").await,
            entity,
        ),
        _ => format!("{} ({entity})", conflict.kind.describe()),
    }
}

async fn entity_label(pool: &SqlitePool, conflict: &Conflict) -> String {
    if let Ok(Some(task)) =
        crate::db::fetch_task_by_id(pool, conflict.entity_id, crate::date::now()).await
    {
        return format!("\"{}\"", task.name);
    }
    format!(
        "{} {}",
        conflict
            .event
            .event
            .payload
            .as_ref()
            .map_or("entity", |payload| payload.kind()),
        conflict.entity_id
    )
}

fn field_label(conflict: &Conflict) -> &'static str {
    match conflict.field {
        Some("name") => "name",
        Some("body") => "note",
        Some("mood") => "mood",
        _ => "text",
    }
}

async fn parent_label(pool: &SqlitePool, task: Option<crate::db::Id>, fallback: &str) -> String {
    let Some(task) = task else {
        return "no parent".to_string();
    };
    match crate::db::fetch_task_by_id(pool, task, crate::date::now()).await {
        Ok(Some(row)) => format!("\"{}\"", row.name),
        _ => fallback.to_string(),
    }
}
