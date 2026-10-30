//! Device identity and the two clock-ish numbers the client keeps.
//!
//! `_sync_state` is a key/value table holding exactly: the device id, the pull
//! cursor, and the last stamp this device emitted. Nothing per-entity lives
//! here: the merge is decided by the server, so the client keeps no watermarks.

use anyhow::{Context, Result};
use sqlx::SqliteConnection;

use crate::db::Id;

pub const KEY_DEVICE_ID: &str = "device_id";
pub const KEY_CURSOR: &str = "cursor";
pub const KEY_LAST_STAMP: &str = "last_own_stamp";
/// Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub async fn get(conn: &mut SqliteConnection, key: &str) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM _sync_state WHERE key = ?")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .with_context(|| format!("Failed to read `{key}` from the sync state"))
}

pub async fn set(conn: &mut SqliteConnection, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO _sync_state (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("Failed to store `{key}` in the sync state"))?;
    Ok(())
}

/// The user id this database last signed in as, if it ever did.
pub async fn account_user_id(conn: &mut SqliteConnection) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT user_id FROM _sync_account WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the synced account")
}

pub async fn token(conn: &mut SqliteConnection) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT token FROM _sync_account WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the session token")
}

/// Remember the account. A `None` user id keeps the one already stored (a
/// device can be signed in with nothing but a token).
pub async fn set_account(
    conn: &mut SqliteConnection,
    user_id: Option<&str>,
    token: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO _sync_account (id, user_id, token) VALUES (1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             user_id = COALESCE(excluded.user_id, _sync_account.user_id),
             token = excluded.token",
    )
    .bind(user_id)
    .bind(token)
    .execute(&mut *conn)
    .await
    .context("Failed to store the synced account")?;
    Ok(())
}

pub async fn clear_account(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query("DELETE FROM _sync_account")
        .execute(&mut *conn)
        .await
        .context("Failed to clear the synced account")?;
    Ok(())
}

pub async fn remove(conn: &mut SqliteConnection, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM _sync_state WHERE key = ?")
        .bind(key)
        .execute(&mut *conn)
        .await
        .context("Failed to clear a sync state value")?;
    Ok(())
}

async fn get_i64(conn: &mut SqliteConnection, key: &str) -> Result<i64> {
    Ok(get(&mut *conn, key)
        .await?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

/// The device id, allocated on first use. It names this copy of the database
/// for echo suppression and error attribution, so a reset rotates it.
pub async fn device_id(conn: &mut SqliteConnection) -> Result<Id> {
    if let Some(existing) = get(&mut *conn, KEY_DEVICE_ID).await?
        && let Ok(id) = existing.parse::<Id>() {
            return Ok(id);
        }
    let id = Id::new();
    set(&mut *conn, KEY_DEVICE_ID, &id.to_string()).await?;
    Ok(id)
}

/// The highest delivered `version` this device has merged.
pub async fn cursor(conn: &mut SqliteConnection) -> Result<i64> {
    get_i64(&mut *conn, KEY_CURSOR).await
}

pub async fn set_cursor(conn: &mut SqliteConnection, cursor: i64) -> Result<()> {
    set(&mut *conn, KEY_CURSOR, &cursor.to_string()).await
}

/// The stamp for the next event this device emits.
///
/// Monotone against this device's own history only: wall clock, but never at or
/// below the last stamp it emitted. That covers two edits inside one
/// millisecond and a backwards NTP step without making a stamp depend on what
/// other devices did.
pub async fn next_stamp(conn: &mut SqliteConnection) -> Result<i64> {
    let last = get_i64(&mut *conn, KEY_LAST_STAMP).await?;
    let stamp = now_ms().max(last.saturating_add(1));
    set(&mut *conn, KEY_LAST_STAMP, &stamp.to_string()).await?;
    Ok(stamp)
}
