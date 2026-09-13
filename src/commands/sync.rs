//! `im sync` — push the queued events and apply the other devices' events.
//!
//! Every event is applied by field-level last-write-wins (§4.1); the one thing
//! the user decides here is a conflict between the incoming event and a change
//! this device has not pushed yet (§4.2). `im sync --reset` throws the local
//! copy away and rebuilds it from the server log (§4.4.2), which is the escape
//! hatch when the local state is diverged or unreadable.

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
/// What to do about local mutations that `--reset` is about to drop.
#[derive(Debug)]
enum ResetGuard {
    /// Nothing unsynced: reset without asking.
    Proceed,
    /// Ask before dropping this many unsynced mutations.
    Confirm(usize),
}

/// The `--reset` tripwire. Dropped mutations exist nowhere but the local
/// database, so a non-interactive run refuses instead of guessing.
fn reset_guard(pending: usize, interactive: bool) -> Result<ResetGuard> {
    if pending == 0 {
        return Ok(ResetGuard::Proceed);
    }
    if !interactive {
        bail!(
            "{pending} local change(s) have not been synced yet and `im sync --reset` would discard \
             them — sync first, or run it in a terminal to confirm"
        );
    }
    Ok(ResetGuard::Confirm(pending))
}

pub async fn sync_command(pool: &SqlitePool, config: &Config, reset: bool) -> Result<()> {
    let server = server_url();
    let slots = TrackerSlots::from_config(config);
    let interactive = atty::is(atty::Stream::Stdin);

    if reset {
        let pending = session::unsynced_count(pool).await?;
        if let ResetGuard::Confirm(pending) = reset_guard(pending, interactive)?
            && !crate::prompts::prompt_discard_unsynced(pending)?
        {
            bail!("keeping the local database");
        }
        let dropped = session::reset_local_state(pool).await?;
        println!("Cleared the local database; rebuilding it from the server log.");
        if dropped > 0 {
            println!("Discarded {dropped} unsynced local change(s).");
        }
    }

    let mut pushed = 0;
    let mut applied = 0;
    let mut stale = 0;
    let mut discarded = 0;
    let mut conflicts = 0;

    for _ in 0..MAX_PAGES {
        let report = sync_once(pool, &server, TIMEOUT, &slots).await?;
        pushed += report.pushed;
        applied += report.applied;
        stale += report.stale;
        discarded += report.discarded;

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
        if discarded > 0 {
            println!("Discarded {discarded} event(s) for entries deleted on another device.");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing unsynced: a reset needs no confirmation.
    #[test]
    fn a_reset_without_pending_changes_proceeds() {
        assert!(matches!(
            reset_guard(0, false).unwrap(),
            ResetGuard::Proceed
        ));
    }

    /// Unsynced changes: a terminal asks, a pipe refuses.
    #[test]
    fn a_reset_with_pending_changes_refuses_without_a_terminal() {
        assert!(matches!(
            reset_guard(3, true).unwrap(),
            ResetGuard::Confirm(3)
        ));
        let err = reset_guard(3, false).unwrap_err().to_string();
        assert!(err.contains("3 local change(s)"), "{err}");
    }
}
