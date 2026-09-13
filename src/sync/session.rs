//! One push+pull round trip, shared by the sync commands.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{SqliteConnection, SqlitePool};

use super::apply::{self, Conflict};
use super::client::{Account, AccountStatus, Client, run_blocking};
use super::state::{self, KEY_AUTH_TOKEN, KEY_LAST_SERVER_VERSION, KEY_USER_ID};
use crate::db::EventId;
use crate::tracker::TrackerSlots;

/// What one round trip did.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Outbox events handed to the server.
    pub pushed: usize,
    /// Remote events applied locally.
    pub applied: usize,
    /// Remote events that were older than what this device already has.
    pub stale: usize,
    /// Events left to the user's decision (see [`Conflict`]).
    pub conflicts: Vec<Conflict>,
    /// The cursor the next sync resumes from.
    pub server_version: i64,
    /// The server holds more events than the page it just returned.
    pub has_more: bool,
}

impl SyncReport {
    pub fn settled(&self) -> bool {
        self.conflicts.is_empty() && !self.has_more
    }
}

/// Read a `_sync_state` value.
pub async fn state_get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::get(&mut conn, key).await
}

/// Write a `_sync_state` value.
pub async fn state_set(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::set(&mut conn, key, value).await
}

/// Clear a `_sync_state` value.
pub async fn state_remove(pool: &SqlitePool, key: &str) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("Failed to acquire a db connection")?;
    state::remove(&mut conn, key).await
}

/// Store the account a successful `im auth register`/`login` returned.
pub async fn store_account(pool: &SqlitePool, account: &Account) -> Result<()> {
    let previous = state_get(pool, KEY_USER_ID).await?;
    if previous.as_deref() != Some(account.user_id.as_str()) {
        // A different account: the version cursor belonged to the previous
        // user's log, so this device starts from that log's beginning.
        state_set(pool, KEY_LAST_SERVER_VERSION, "0").await?;
    }
    state_set(pool, KEY_USER_ID, &account.user_id).await?;
    state_set(pool, KEY_AUTH_TOKEN, &account.token).await?;
    Ok(())
}

/// The signed-in account, if any.
pub async fn signed_in(pool: &SqlitePool) -> Result<Option<String>> {
    state_get(pool, KEY_AUTH_TOKEN).await
}

/// Push the outbox and apply one pulled page.
///
/// The page commits as a unit together with the cursor (`@@SYNC.md` §4.5), so
/// a conflict skips only its own event: everything else in the page lands and
/// the caller asks the user before the next request.
pub async fn sync_once(
    pool: &SqlitePool,
    server: &str,
    timeout: Duration,
    slots: &TrackerSlots,
) -> Result<SyncReport> {
    let token = signed_in(pool)
        .await?
        .context("not logged in — run `im auth login`")?;
    let device = {
        let mut conn = pool
            .acquire()
            .await
            .context("Failed to acquire a db connection")?;
        state::device_id(&mut conn).await?
    };
    let since = state_get(pool, KEY_LAST_SERVER_VERSION)
        .await?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let events = apply::pending_events(pool).await?;
    let pushed: Vec<EventId> = events.iter().map(|event| event.event_id).collect();

    let client = Client::with_timeout(server, Some(token), timeout);
    let request_client = client.clone();
    let outcome = run_blocking(move || request_client.sync(device, since, &events)).await?;
    // The page is replayed before the push counts as acknowledged: an incoming
    // event that contradicts a decision made in this same round is still a
    // concurrent decision, and the user settles it (`@@SYNC.md` §4.4). Marking
    // late is harmless — a crash here only re-pushes, and `event_id` is unique
    // server-side.
    let page = apply::apply_page(
        pool,
        &outcome.remote_events,
        outcome.new_server_version,
        slots,
    )
    .await?;
    apply::mark_pushed(pool, &pushed).await?;

    let report = SyncReport {
        pushed: pushed.len(),
        applied: page.applied,
        stale: page.stale,
        conflicts: page.conflicts,
        server_version: outcome.new_server_version,
        has_more: outcome.has_more,
    };
    Ok(report)
}

/// Resolve one conflict and keep the local side consistent.
pub async fn resolve(
    pool: &SqlitePool,
    conflict: &Conflict,
    resolution: super::apply::Resolution,
) -> Result<()> {
    apply::resolve_conflict(pool, conflict, resolution).await
}

/// The connection-level device id (for callers that already hold one).
pub async fn device_id_of(conn: &mut SqliteConnection) -> Result<crate::db::Id> {
    state::device_id(conn).await
}

/// Create an account (blocking HTTP, off the async worker threads).
pub async fn register(base: &str, email: &str, password: &str) -> Result<Account> {
    let client = Client::new(base, None);
    let (email, password) = (email.to_string(), password.to_string());
    run_blocking(move || client.register(&email, &password)).await
}

/// Sign in to an existing account.
pub async fn login(base: &str, email: &str, password: &str) -> Result<Account> {
    let client = Client::new(base, None);
    let (email, password) = (email.to_string(), password.to_string());
    run_blocking(move || client.login(&email, &password)).await
}

/// The account a stored token belongs to.
pub async fn account_status(base: &str, token: &str) -> Result<AccountStatus> {
    let client = Client::new(base, Some(token.to_string()));
    run_blocking(move || client.status()).await
}
