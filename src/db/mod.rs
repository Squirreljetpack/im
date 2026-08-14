use anyhow::{Context, Result};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;


pub async fn init_database(db_path: &Path) -> anyhow::Result<SqlitePool> {
    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

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
                return Err(e).with_context(|| format!("Failed to remove {}", path.display()))
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
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            mood TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            time INTEGER NOT NULL DEFAULT (unixepoch()),
            embedding BLOB,
            -- Cached emotional-saliency score for the mood text (nullable;
            -- backfilled by mood_color_cached). No migration: an existing
            -- DB without the column is deleted by the user.
            score REAL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tracker (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            -- BLOB decltype = no type affinity: storage class is preserved
            -- exactly (integer/text/real) so sqlx can decode by value type.
            score BLOB NOT NULL CHECK (typeof(score) IN ('integer', 'text', 'real')),
            time INTEGER NOT NULL DEFAULT (unixepoch()),
            mood INTEGER,
            FOREIGN KEY (mood) REFERENCES mood(id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            priority INTEGER NOT NULL DEFAULT 5,
            -- User-facing short id: allocated by the db layer (first free
            -- gap); NULL once the task is completed (oneshot) or for
            -- recurring tasks done in the current interval.
            short_id INTEGER UNIQUE,
            -- Reserved for a name-derived embedding; never populated.
            name_embedding BLOB,
            start_time INTEGER,
            available_duration_secs INTEGER,
            interval_secs INTEGER,
            target_count INTEGER NOT NULL DEFAULT 0,
            optional INTEGER NOT NULL DEFAULT 0,
            end_time INTEGER,
            -- Parent task id for the task tree (NULL = root-level task).
            -- Deleting a parent re-parents its children to root level
            -- (ON DELETE SET NULL) rather than cascading or failing.
            parent INTEGER REFERENCES todos(id) ON DELETE SET NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todo_completions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            todo_id INTEGER NOT NULL,
            time INTEGER NOT NULL DEFAULT (unixepoch()),
            count INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY (todo_id) REFERENCES todos(id) ON DELETE CASCADE
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

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS task_moods (
            todo_id INTEGER NOT NULL,
            mood_id INTEGER NOT NULL,
            PRIMARY KEY (todo_id, mood_id),
            FOREIGN KEY (todo_id) REFERENCES todos(id) ON DELETE CASCADE,
            FOREIGN KEY (mood_id) REFERENCES mood(id) ON DELETE CASCADE
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Add indexes for common queries
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_mood_time ON mood(time)")
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

    log::debug!("Database migrations completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
mod models;
mod tasks;
mod views;

pub use embeddings::*;
pub use entries::*;
pub use models::*;
pub use tasks::*;
pub use views::*;
