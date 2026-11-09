//! Device identity, session credentials, and the two clock-ish numbers the
//! client keeps.
//!
//! `_sync_state` is a key/value table holding the device id, the pull cursor,
//! and the last stamp this device emitted. `_sync_account` holds the active
//! session token and user id.

use anyhow::{Context, Result};
use sqlx::{SqliteConnection, SqlitePool};

use super::client::Account;
use super::crypto::EncryptionKey;
use super::types::Cursor;
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

pub async fn token(conn: &mut SqliteConnection) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT token FROM _sync_account WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .context("Failed to read the session token")
}

pub async fn encryption_key(conn: &mut SqliteConnection) -> Result<Option<EncryptionKey>> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT encryption_key FROM _sync_account WHERE id = 1")
            .fetch_optional(&mut *conn)
            .await
            .context("Failed to read the encryption key")?;
    match raw {
        Some(encoded) => Ok(Some(EncryptionKey::from_base64(&encoded)?)),
        None => Ok(None),
    }
}

/// Remember the account.
pub async fn set_account(
    conn: &mut SqliteConnection,
    user_id: Option<&str>,
    token: &str,
    encryption_key: &EncryptionKey,
) -> Result<()> {
    let enc_b64 = encryption_key.to_base64();
    sqlx::query(
        "INSERT INTO _sync_account (id, user_id, token, encryption_key) VALUES (1, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             user_id = COALESCE(excluded.user_id, _sync_account.user_id),
             token = excluded.token,
             encryption_key = excluded.encryption_key",
    )
    .bind(user_id)
    .bind(token)
    .bind(enc_b64)
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
        && let Ok(id) = existing.parse::<Id>()
    {
        return Ok(id);
    }
    let id = Id::new();
    set(&mut *conn, KEY_DEVICE_ID, &id.to_string()).await?;
    Ok(id)
}

/// The log this database has read to, and how far. `None` until a sync names
/// one.
pub async fn cursor(conn: &mut SqliteConnection) -> Result<Option<Cursor>> {
    Ok(get(&mut *conn, KEY_CURSOR)
        .await?
        .as_deref()
        .and_then(Cursor::parse))
}

pub async fn set_cursor(conn: &mut SqliteConnection, cursor: Option<&Cursor>) -> Result<()> {
    match cursor {
        Some(cursor) => set(&mut *conn, KEY_CURSOR, &cursor.encode()).await,
        None => remove(&mut *conn, KEY_CURSOR).await,
    }
}

/// Read the pull cursor from a pool connection.
pub async fn pool_cursor(pool: &SqlitePool) -> Result<Option<Cursor>> {
    let mut conn = pool.acquire().await?;
    cursor(&mut conn).await
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

/// Reset the last-emitted stamp after the server reports `clock_ahead`.
///
/// Clamping `KEY_LAST_STAMP` back to `server_now + n` means that if this device
/// previously pushed cells with stamps inside `(server_now, server_now + SKEW_MS]`
/// that the server accepted, a subsequent edit to one of those cells before wall
/// time catches up can carry a smaller stamp and lose LWW to the device's own
/// pre-correction write (producing a supersession notice, then self-healing on
/// the next edit after the skew window passes).
pub async fn reset_clock(conn: &mut SqliteConnection, stamp: i64) -> Result<()> {
    set(conn, KEY_LAST_STAMP, &stamp.to_string()).await
}

/// Store the authenticated account and the encryption key this device will use.
///
/// An existing pull cursor is intentionally preserved: if the database was
/// previously synced against another account's log, the next sync will detect
/// `LogMismatch` and require an explicit `--reset` rather than silently merging
/// two accounts' histories.
pub async fn store_account(
    pool: &SqlitePool,
    account: &Account,
    encryption_key: &EncryptionKey,
) -> Result<()> {
    let mut conn = pool.acquire().await.context("Failed to acquire a db connection")?;
    set_account(
        &mut conn,
        Some(&account.user_id),
        &account.token,
        encryption_key,
    )
    .await
}

/// The signed-in account's token, if any.
pub async fn signed_in(pool: &SqlitePool) -> Result<Option<String>> {
    let mut conn = pool.acquire().await.context("Failed to acquire a db connection")?;
    token(&mut conn).await
}

/// Clear the stored session credentials.
pub async fn logout(pool: &SqlitePool) -> Result<()> {
    let mut conn = pool.acquire().await.context("Failed to acquire a db connection")?;
    clear_account(&mut conn).await
}
