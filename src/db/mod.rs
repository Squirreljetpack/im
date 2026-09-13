use anyhow::{Context, Result};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnection, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An opened database: the pool plus, when the file still held the pre-sync
/// integer-key schema, the backup taken before the schema was recreated.
pub struct OpenedDatabase {
    pub pool: SqlitePool,
    /// Path of the `VACUUM INTO` copy of a legacy database (see
    /// [`back_up_legacy_database`]); `None` for a fresh or current-schema
    /// file.
    pub legacy_backup: Option<PathBuf>,
}

pub async fn init_database(db_path: &Path) -> anyhow::Result<OpenedDatabase> {
    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // A file from an older version still has integer primary keys. Copy it
    // aside and remove it before opening: the schema below is created with
    // `IF NOT EXISTS`, so an existing legacy table would silently survive
    // with the wrong key type.
    let legacy_backup = back_up_legacy_database(db_path).await?;

    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        // An invalid db file fails the after_connect PRAGMAs below; the pool
        // retries acquires until the timeout, so without this cap opening a
        // corrupt db stalls every command for the 30s default. Normal
        // acquires are milliseconds — 5s only matters for the broken case.
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|conn, _| {
            Box::pin(async move {
                // WAL leaves -wal/-shm sidecar files; only enable it in
                // release builds so dev runs don't litter the state dir.
                // (Setting DELETE explicitly in debug also converts a
                // pre-existing WAL-mode db file, so the mode is deterministic.)
                #[cfg(debug_assertions)]
                sqlx::query("PRAGMA journal_mode = DELETE;")
                    .execute(&mut *conn)
                    .await?;
                #[cfg(not(debug_assertions))]
                sqlx::query("PRAGMA journal_mode = WAL;")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA synchronous = NORMAL;")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA foreign_keys = ON;")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&db_url)
        .await?;

    run_migrations(&pool).await?;

    log::debug!("Database initialized at {:?}", db_path);
    Ok(OpenedDatabase {
        pool,
        legacy_backup,
    })
}

/// Backup path for a legacy database: `<file>.legacy-<unix-seconds>.bak`.
fn legacy_backup_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    name.push(format!(".legacy-{secs}.bak"));
    PathBuf::from(name)
}

/// Whether an existing database file uses the pre-sync integer-key schema
/// (`todos.id` declared `INTEGER`). A file without a `todos` table is not
/// legacy — it is either fresh or half-built.
async fn has_legacy_schema(db_path: &Path) -> Result<bool> {
    let db_url = format!("sqlite:{}?mode=rw", db_path.display());
    let mut conn = SqliteConnection::connect(&db_url)
        .await
        .with_context(|| format!("Failed to open {} for inspection", db_path.display()))?;
    let declared: Option<String> =
        sqlx::query_scalar("SELECT type FROM pragma_table_info('todos') WHERE name = 'id'")
            .fetch_optional(&mut conn)
            .await
            .with_context(|| format!("Failed to inspect the schema of {}", db_path.display()))?;
    conn.close()
        .await
        .with_context(|| format!("Failed to close {} after inspection", db_path.display()))?;
    Ok(declared.is_some_and(|decl_type| decl_type.eq_ignore_ascii_case("INTEGER")))
}

/// Copy a legacy (integer-key) database aside with `VACUUM INTO` — a single
/// consistent file, WAL contents included — then delete it so the caller
/// starts from the current schema with no rows. Returns the backup path, or
/// `None` when the file is absent or already uses UUID keys.
async fn back_up_legacy_database(db_path: &Path) -> Result<Option<PathBuf>> {
    if !db_path.is_file() || !has_legacy_schema(db_path).await? {
        return Ok(None);
    }

    let backup = legacy_backup_path(db_path);
    let escaped = backup.display().to_string().replace('\'', "''");
    let db_url = format!("sqlite:{}?mode=rw", db_path.display());
    let mut conn = SqliteConnection::connect(&db_url)
        .await
        .with_context(|| format!("Failed to open {} for backup", db_path.display()))?;
    sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{escaped}'")))
        .execute(&mut conn)
        .await
        .with_context(|| {
            format!(
                "Failed to back up {} to {}",
                db_path.display(),
                backup.display()
            )
        })?;
    conn.close().await?;

    delete_database(db_path)?;
    log::warn!(
        "Recreated the database without the rows of the pre-sync schema; the old file was copied to {}",
        backup.display()
    );
    Ok(Some(backup))
}

/// Delete the database file and its `-wal`/`-shm` sidecar files (if any).
/// Called after the user confirms removing an invalid database so a fresh
/// one can be initialized in its place. Missing files (including sidecars
/// from a WAL-mode db) are not an error.
pub fn delete_database(db_path: &Path) -> Result<()> {
    let mut targets = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        targets.push(PathBuf::from(name));
    }

    for path in targets {
        match std::fs::remove_file(&path) {
            Ok(()) => log::debug!("Removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

/// Create an in-memory SQLite pool for testing.
pub async fn test_pool() -> anyhow::Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("PRAGMA foreign_keys = ON;")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect("sqlite::memory:")
        .await?;

    run_migrations(&pool).await?;

    Ok(pool)
}
pub async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    // Create tables if they don't exist
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS mood (
            id        TEXT PRIMARY KEY,
            mood      TEXT NOT NULL,
            body      TEXT NOT NULL DEFAULT '',
            time      INTEGER NOT NULL DEFAULT (unixepoch()),
            -- Local cache only: the ONNX embedding is never synced.
            embedding BLOB,
            -- Cached emotional-saliency score for the mood text (nullable;
            -- backfilled by mood_color_cached).
            score REAL,
            duration INTEGER,
            -- Optional link to a single task (1 task per mood).
            todo_id   TEXT REFERENCES todos(id) ON DELETE SET NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tracker (
            id    TEXT PRIMARY KEY,
            type  TEXT NOT NULL,
            -- BLOB decltype = no type affinity: storage class is preserved
            -- exactly (integer/text/real) so sqlx can decode by value type.
            score BLOB NOT NULL CHECK (typeof(score) IN ('integer', 'text', 'real')),
            time  INTEGER NOT NULL DEFAULT (unixepoch()),
            mood  TEXT REFERENCES mood(id) ON DELETE SET NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todos (
            id                      TEXT PRIMARY KEY,
            name                    TEXT NOT NULL,
            body                    TEXT NOT NULL DEFAULT '',
            priority                INTEGER NOT NULL DEFAULT 5,
            -- User-facing short id: local presentation only (never synced),
            -- allocated by the db layer (first free gap); NULL once the task
            -- is completed (oneshot) or for recurring tasks done in the
            -- current interval.
            short_id                INTEGER UNIQUE,
            -- Reserved for a name-derived embedding; local only.
            name_embedding          BLOB,
            start_time              INTEGER,
            available_duration_secs INTEGER,
            interval_secs           INTEGER,
            target_count            INTEGER NOT NULL DEFAULT 0,
            optional                INTEGER NOT NULL DEFAULT 0,
            end_time                INTEGER,
            -- Parent task id for the task tree (NULL = root-level task).
            -- Deleting a parent re-parents its children to root level
            -- (ON DELETE SET NULL) rather than cascading or failing.
            parent                  TEXT REFERENCES todos(id) ON DELETE SET NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todo_completions (
            id      TEXT PRIMARY KEY,
            todo_id TEXT NOT NULL REFERENCES todos(id) ON DELETE CASCADE,
            time    INTEGER NOT NULL DEFAULT (unixepoch()),
            count   INTEGER NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS embedding_cache (
            text TEXT PRIMARY KEY,
            embedding BLOB NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Sync engine: credentials and cursors ('user_id', 'device_id',
    // 'auth_token', 'last_server_version').
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sync_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Sync engine: append-only local event history. `synced = 0` rows are the
    // outbox awaiting push; `payload` is the serialized `Option<EntityPayload>`
    // (SQL NULL serialized as JSON `null` = delete).
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sync_events (
            version    INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id   TEXT NOT NULL UNIQUE,
            device_id  TEXT NOT NULL,
            entity_id  TEXT NOT NULL,
            timestamp  INTEGER NOT NULL,
            payload    TEXT NOT NULL,
            synced     INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Sync engine: which event last won each *field* of an entity. LWW is
    // decided per field, so two devices that edited different fields of one row
    // keep both edits (§4.1) while a shared field still converges. `field = ''`
    // is the entity itself: the stamp of the last event that changed anything,
    // which the tracker slot cleanup ranks on. No field values are kept: a
    // deletion is terminal, so nothing is ever rebuilt from here.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sync_watermark (
            entity_id TEXT NOT NULL,
            field     TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            device_id TEXT NOT NULL,
            -- The applied event's id: the final LWW tie-breaker, so two
            -- machines sharing a copied device id still converge.
            event_id  TEXT NOT NULL,
            PRIMARY KEY (entity_id, field)
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Sync engine: every entity this device has seen, its kind and whether it
    // is deleted here. The kind identifies an id whose row is gone.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sync_entities (
            entity_id TEXT PRIMARY KEY,
            kind      TEXT NOT NULL,
            deleted   INTEGER NOT NULL DEFAULT 0
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Add indexes for common queries
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_mood_time ON mood(time)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_mood_todo_id ON mood(todo_id)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tracker_time ON tracker(time)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tracker_mood ON tracker(mood)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tracker_type ON tracker(type)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_todos_short_id ON todos(short_id)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_todos_interval ON todos(interval_secs)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_todos_start_time ON todos(start_time)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_todos_parent ON todos(parent)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_todo_completions_todo_id ON todo_completions(todo_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_todo_completions_todo_time ON todo_completions(todo_id, time)")
        .execute(pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sync_events_synced ON _sync_events(synced)")
        .execute(pool)
        .await?;

    log::debug!("Database migrations completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A db still holding the pre-sync integer-key schema is copied aside
    /// with its rows and recreated empty with TEXT UUID keys.
    #[tokio::test]
    async fn legacy_db_is_backed_up_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("im.db");
        {
            let url = format!("sqlite:{}?mode=rwc", db_path.display());
            let mut conn = SqliteConnection::connect(&url).await.unwrap();
            sqlx::query(
                "CREATE TABLE todos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            sqlx::query("INSERT INTO todos (name) VALUES ('old task')")
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
        }

        let opened = init_database(&db_path).await.unwrap();
        let backup = opened
            .legacy_backup
            .expect("a pre-sync schema must be backed up");
        assert!(backup.is_file(), "backup {} must exist", backup.display());

        // The backup kept the old rows...
        let url = format!("sqlite:{}?mode=rw", backup.display());
        let mut conn = SqliteConnection::connect(&url).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM todos")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(name, "old task");
        conn.close().await.unwrap();

        // ...and the reopened database starts empty with TEXT keys.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
            .fetch_one(&opened.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let declared: String =
            sqlx::query_scalar("SELECT type FROM pragma_table_info('todos') WHERE name = 'id'")
                .fetch_one(&opened.pool)
                .await
                .unwrap();
        assert_eq!(declared, "TEXT");
        opened.pool.close().await;
    }

    /// A garbage db file must fail initialization quickly — the pool's
    /// acquire timeout caps it instead of the 30s default hang.
    #[tokio::test]
    async fn init_fails_fast_on_invalid_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("invalid.db");
        std::fs::write(&db_path, b"not a sqlite database").unwrap();

        let start = std::time::Instant::now();
        let result = init_database(&db_path).await;
        assert!(result.is_err(), "garbage db must fail initialization");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "invalid db must fail fast, not hang on the pool acquire timeout"
        );
    }

    /// A fresh path is created (parent dirs included) and initialized.
    #[tokio::test]
    async fn init_creates_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("fresh.db");

        let pool = init_database(&db_path).await.unwrap().pool;
        pool.close().await;
        assert!(db_path.exists(), "db file must be created");
    }

    /// Deleting an invalid db also removes its WAL/shm sidecars.
    #[test]
    fn delete_database_removes_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("im.db");
        std::fs::write(&db_path, b"x").unwrap();
        std::fs::write(dir.path().join("im.db-wal"), b"x").unwrap();
        std::fs::write(dir.path().join("im.db-shm"), b"x").unwrap();

        delete_database(&db_path).unwrap();
        assert!(!db_path.exists());
        assert!(!dir.path().join("im.db-wal").exists());
        assert!(!dir.path().join("im.db-shm").exists());
    }

    /// Missing files (e.g. a db without WAL sidecars) are not an error.
    #[test]
    fn delete_database_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        delete_database(&dir.path().join("never-existed.db")).unwrap();
    }
}

mod embeddings;
mod entries;
mod ids;
mod models;
mod tasks;
mod views;

pub use embeddings::*;
pub use entries::*;
pub use ids::*;
pub use models::*;
pub use tasks::*;
pub use views::*;
