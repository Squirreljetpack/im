use anyhow::{Context, Result};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnection, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Open the database, replacing an unreadable file with a fresh one
/// (`@@SYNC.md` §4.4). Returns the pool the rest of the run uses.
pub async fn init_database(db_path: &Path) -> Result<SqlitePool> {
    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match open_database(db_path).await {
        Ok(pool) => Ok(pool),
        Err(err) if is_corruption(&err) => {
            // Nothing inside the file is readable, so it cannot be repaired in
            // place: keep the bytes for inspection, then start over. The
            // credentials went to the quarantine copy with everything else, so
            // syncing resumes after `im auth login`.
            let quarantine = quarantine_database(db_path)?;
            let pool = open_database(db_path).await?;
            cba::ebog!(
                "The database was unreadable; the damaged file was copied to {} and a fresh one was initialized. Sign in again to resume syncing.",
                quarantine.display()
            );
            Ok(pool)
        }
        Err(err) => Err(err),
    }
}

/// Open, probe and migrate the database file.
async fn open_database(db_path: &Path) -> Result<SqlitePool> {
    probe_database(db_path).await?;

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
    Ok(pool)
}

/// Read the schema over a plain connection: a file that is not a SQLite
/// database fails here at once, while a pool would retry its `after_connect`
/// hooks until the acquire timeout.
async fn probe_database(db_path: &Path) -> Result<()> {
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let mut conn = SqliteConnection::connect(&db_url)
        .await
        .with_context(|| format!("Failed to open {}", db_path.display()))?;
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master")
        .fetch_one(&mut conn)
        .await
        .with_context(|| format!("Failed to read the schema of {}", db_path.display()))?;
    conn.close()
        .await
        .with_context(|| format!("Failed to close {}", db_path.display()))?;
    Ok(())
}

/// Whether an error means the file is not a readable SQLite database.
fn is_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let Some(sqlx::Error::Database(db)) = cause.downcast_ref::<sqlx::Error>() else {
            return false;
        };
        if db
            .code()
            .is_some_and(|code| code.starts_with("SQLITE_CORRUPT") || code == "SQLITE_NOTADB")
        {
            return true;
        }
        let message = db.message().to_lowercase();
        message.contains("not a database") || message.contains("malformed")
    })
}

/// Copy the database and its sidecars to `<file>.corrupt-<unix-seconds>.bak`
/// and remove the originals. Nothing is removed unless every existing file was
/// copied first, so a failed copy never costs the user the database.
fn quarantine_database(db_path: &Path) -> Result<PathBuf> {
    let backup = with_suffix(db_path, &format!(".corrupt-{}.bak", unix_seconds()));
    for suffix in ["", "-wal", "-shm"] {
        let source = with_suffix(db_path, suffix);
        if !source.is_file() {
            continue;
        }
        let target = with_suffix(&backup, suffix);
        std::fs::copy(&source, &target).with_context(|| {
            format!(
                "Failed to copy the damaged database {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }
    delete_database(db_path)?;
    Ok(backup)
}

/// Seconds since the Unix epoch, for naming backups.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// `<path><suffix>`: the shape SQLite uses for a file and its sidecars.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Delete the database file and its `-wal`/`-shm` sidecar files (if any).
/// Missing files (including sidecars from a WAL-mode db) are not an error.
pub fn delete_database(db_path: &Path) -> Result<()> {
    let targets = ["", "-wal", "-shm"].map(|suffix| with_suffix(db_path, suffix));

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

    /// The quarantine copies in a directory, newest name first.
    fn quarantined(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.to_string_lossy().contains(".corrupt-"))
            .collect();
        found.sort();
        found
    }

    /// An unreadable db file is copied aside and replaced by a fresh one
    /// (§4.4): the bytes survive for inspection, the schema is rebuilt.
    #[tokio::test]
    async fn a_corrupt_database_is_quarantined_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("im.db");
        std::fs::write(&db_path, b"not a sqlite database").unwrap();
        std::fs::write(dir.path().join("im.db-wal"), b"trailing frames").unwrap();

        let start = std::time::Instant::now();
        let pool = init_database(&db_path).await.unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a corrupt db must be recognized without waiting for the pool timeout"
        );

        // The db file is kept as `<file>.corrupt-<unix>.bak`. Its sidecar is
        // copied next to it when SQLite has not already dropped the stale
        // `-wal` while probing the file.
        let backups: Vec<PathBuf> = quarantined(dir.path());
        assert!(!backups.is_empty(), "the damaged file is kept aside");
        let db_backup = backups
            .iter()
            .find(|path| path.to_string_lossy().ends_with(".bak"))
            .expect("the db copy keeps the plain .bak name");
        assert_eq!(std::fs::read(db_backup).unwrap(), b"not a sqlite database");

        // The fresh database is usable and starts empty.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        pool.close().await;

        // Reopening the healthy file quarantines nothing further.
        let pool = init_database(&db_path).await.unwrap();
        pool.close().await;
        assert_eq!(quarantined(dir.path()).len(), backups.len());
    }

    /// A fresh path is created (parent dirs included) and initialized.
    #[tokio::test]
    async fn init_creates_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("fresh.db");

        let pool = init_database(&db_path).await.unwrap();
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
