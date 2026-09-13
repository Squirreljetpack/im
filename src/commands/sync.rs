//! `im sync` — push the queued events and apply the other devices' events.
//!
//! Replaying an incoming delete that contradicts a local edit is the one
//! thing the user has to decide (§4.4); every other event is applied or
//! skipped by last-write-wins.

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
                let resolution = prompt_resolution(conflict)?;
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

fn prompt_resolution(conflict: &Conflict) -> Result<Resolution> {
    let mut prompt = cliclack::select(format!(
        "{} ({})",
        conflict.kind.describe(),
        conflict.entity_id
    ))
    .item(Resolution::ConfirmRemote, conflict.kind.confirm_label(), "");
    if conflict.resurrectable {
        prompt = prompt.item(Resolution::Resurrect, conflict.kind.resurrect_label(), "");
    }
    prompt
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))
}
