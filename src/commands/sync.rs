//! `im :sync` — push the outbox and merge what the other devices wrote.
//!
//! Nothing here arbitrates: the server already named the winner of every cell,
//! so a round applies values and reports the decisions it had to take on the
//! user's behalf. The one question it asks is whether to re-create content that
//! was deleted elsewhere. `im :sync --reset` throws the local copy away and
//! rebuilds it from the register, which is the escape hatch when the local
//! state is wrong rather than merely old.
//!
//! The account subcommands (`register`, `login`, `status`, `logout`) live here
//! too: the account exists only to make syncing possible.

use std::time::Duration;

use anyhow::{Result, bail};
use sqlx::SqlitePool;

use crate::cli::SyncSubcommand;
use crate::config::Config;
use crate::sync::client::server_url;
use crate::sync::session::{self, Resurrect, sync_once};
use crate::tracker::TrackerSlots;

/// `im :sync` — sync, or manage the account this device syncs with.
pub(super) async fn sync_command(
    pool: &SqlitePool,
    config: &Config,
    sub: Option<SyncSubcommand>,
    reset: bool,
    force: bool,
) -> Result<()> {
    match sub {
        None => run_sync(pool, config, reset, force).await,
        Some(SyncSubcommand::Register) => register(pool, config).await,
        Some(SyncSubcommand::Login) => login(pool, config).await,
        Some(SyncSubcommand::Status) => status(pool, config).await,
        Some(SyncSubcommand::Logout) => logout(pool).await,
    }
}

/// How long `im :sync` waits on the server: it is the interactive path, so a
/// slow connection is worth waiting for.
const TIMEOUT: Duration = Duration::from_secs(30);

/// `--reset` must not silently discard changes that never reached the server.
/// Rebuilding is only safe when nothing outside the server's log would be lost,
/// so the prompt appears exactly when something would be.
async fn confirm_reset(pool: &SqlitePool, interactive: bool, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let pending = session::unsynced_count(pool).await?;
    if pending == 0 {
        return Ok(());
    }
    if !interactive {
        bail!(
            "{pending} local change(s) have not been synced yet and `im :sync --reset` would discard \
             them — sync first, or run it in a terminal to confirm, or pass --force"
        );
    }
    if !crate::prompts::prompt_discard_unsynced(pending)? {
        bail!("keeping the local database");
    }
    Ok(())
}

async fn run_sync(pool: &SqlitePool, config: &Config, reset: bool, force: bool) -> Result<()> {
    let server = server_url(config);
    let slots = TrackerSlots::from_config(config);
    let interactive = atty::is(atty::Stream::Stdin);

    if reset {
        confirm_reset(pool, interactive, force).await?;
        let dropped = session::reset_local_state(pool).await?;
        println!("Cleared the local database; rebuilding it from the server.");
        if dropped > 0 {
            println!("Discarded {dropped} unsynced local change(s).");
        }
    }

    let mut pull = session::Pull::resume(pool).await?;
    let mut totals = Totals::default();
    let mut clock_retried = false;
    loop {
        let report = sync_once(pool, &server, TIMEOUT, &slots, &mut pull).await?;
        totals.pushed += report.pushed;
        totals.created += report.created;
        totals.applied += report.applied;
        totals.deleted += report.deleted;

        for notice in &report.notices {
            println!("{notice}");
        }
        for skip in &report.skipped {
            eprintln!(
                "skipped a cell of {} ({}): {}",
                skip.entity_id, skip.field, skip.reason
            );
        }

        if let Some(now) = report.clock_ahead {
            let ahead = crate::sync::state::now_ms() - now;
            if clock_retried {
                bail!(
                    "this device's clock is {} ahead of the server, so its writes keep being refused — \
                     fix the clock and run `im :sync` again",
                    crate::date::format_duration(ahead / 1000)
                );
            }
            println!(
                "This device's clock is {} ahead of the server; retrying once with fresh stamps.",
                crate::date::format_duration(ahead / 1000)
            );
            clock_retried = true;
            continue;
        }

        if report.has_more {
            // Termination rests on this: the server only reports another page
            // after delivering cells, so the cursor must have moved. A boundary
            // that did not move it would re-issue the same request forever.
            anyhow::ensure!(
                report.cursor > report.cursor_before,
                "the server reported more pages but the cursor did not advance past {}; stopping",
                report.cursor_before
            );
            continue;
        }
        if report.pending > 0 {
            // Applying a page can queue repairs of its own (a tracker slot loser
            // is deleted and pushed as a delete), so a round can end with work
            // left over. Each repair deletes a local row, so draining converges;
            // a round that could not push what is queued would not.
            anyhow::ensure!(
                report.pushed > 0,
                "{} change(s) are queued but the outbox returned none of them; stopping",
                report.pending
            );
            continue;
        }
        for offer in &report.resurrect {
            maybe_resurrect(pool, offer, interactive).await?;
        }

        println!(
            "Pushed {} change(s); applied {}; created {} row(s); deleted {}.",
            totals.pushed, totals.applied, totals.created, totals.deleted
        );
        println!("Up to date at cursor {}.", report.cursor);
        return Ok(());
    }
}

#[derive(Default)]
struct Totals {
    pushed: usize,
    created: usize,
    applied: usize,
    deleted: usize,
}

/// Offer a copy of something another device deleted.
async fn maybe_resurrect(pool: &SqlitePool, offer: &Resurrect, interactive: bool) -> Result<()> {
    let name = match offer.kind.as_str() {
        "task" => "task",
        "mood" => "mood entry",
        _ => return Ok(()),
    };
    let when = offer
        .at
        .map(|stamp| crate::date::format_human_datetime(stamp / 1000, false))
        .unwrap_or_else(|| "an unknown time".to_string());
    let device = offer.device.as_deref().unwrap_or("another device");
    if !interactive {
        eprintln!(
            "A {name} was deleted on {device} at {when}; run `im :sync` in a terminal to keep a copy."
        );
        return Ok(());
    }
    if !cliclack::confirm(format!(
        "A {name} was deleted on {device} at {when}. Re-create it as a copy?"
    ))
    .initial_value(false)
    .interact()?
    {
        return Ok(());
    }
    match session::resurrect(pool, offer).await? {
        Some(id) => println!("Re-created it as {}.", &id.to_string()[..8]),
        None => println!("Nothing was left to copy."),
    }
    Ok(())
}

/// `:sync register` — create an account and sign this device in.
async fn register(pool: &SqlitePool, config: &Config) -> Result<()> {
    let (email, password) = prompt_credentials()?;
    let server = server_url(config);
    let account = session::register(&server, &email, &password).await?;
    session::store_account(pool, &account).await?;
    println!(
        "Registered {} on {server}; this device is signed in.",
        account.email
    );
    println!("Run `im :sync` to upload the events this device has queued.");
    Ok(())
}

/// `:sync login` — sign this device in to an existing account.
async fn login(pool: &SqlitePool, config: &Config) -> Result<()> {
    let (email, password) = prompt_credentials()?;
    let server = server_url(config);
    let account = session::login(&server, &email, &password).await?;
    session::store_account(pool, &account).await?;
    println!("Signed in as {} on {server}.", account.email);
    println!("Run `im :sync` to exchange events with the other devices.");
    Ok(())
}

/// `:sync status` — show the account this device is signed in as.
async fn status(pool: &SqlitePool, config: &Config) -> Result<()> {
    let Some(token) = session::signed_in(pool).await? else {
        println!("Not signed in (run `im :sync login`).");
        return Ok(());
    };
    let server = server_url(config);
    match session::account_status(&server, &token).await {
        Ok(status) => {
            let queued = session::unsynced_count(pool).await?;
            println!(
                "Signed in as {} ({}) on {server}.",
                status.email, status.user_id
            );
            println!("{}", describe_cursor(pool).await?);
            println!("{queued} local change(s) queued to push.");
        }
        Err(error) => {
            println!("Signed in locally, but {server} rejected the session: {error}")
        }
    }
    Ok(())
}

/// Where this database has read to, plus the versions this build speaks — the
/// numbers a refusal quotes back.
async fn describe_cursor(pool: &SqlitePool) -> Result<String> {
    let cursor = session::cursor(pool).await?;
    let versions = format!(
        "protocol {}, schema {}",
        crate::sync::schema::SCHEMA.protocol,
        crate::sync::schema::SCHEMA.version
    );
    Ok(match cursor {
        Some(cursor) => {
            let log: String = cursor.log.chars().take(8).collect();
            format!(
                "Synced to log {log} at version {} ({versions}).",
                cursor.version
            )
        }
        None => format!("Not synced to any log yet ({versions})."),
    })
}

/// `:sync logout` — forget the account on this device (local data stays).
async fn logout(pool: &SqlitePool) -> Result<()> {
    // The server keeps the events it already has; this device keeps
    // its data and simply stops syncing it.
    session::logout(pool).await?;
    println!("Signed out. Local data is untouched.");
    Ok(())
}

fn prompt_credentials() -> Result<(String, String)> {
    if !atty::is(atty::Stream::Stdin) {
        bail!("`im :sync` needs an interactive terminal to ask for the email and password");
    }
    let email: String = cliclack::input("Email:")
        .placeholder("you@example.com")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))?;
    let password: String = cliclack::password("Password:")
        .mask('*')
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))?;
    Ok((email.trim().to_string(), password))
}
