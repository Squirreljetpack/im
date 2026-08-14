//! Process-wide state: the bundled embedding model, the config the TUI
//! apps were built with, and the database pool.
//!
//! The embedder lives here because the ONNX model is compiled into the
//! binary and shared everywhere. The config and pool are installed once the
//! TUI layer starts (see [`GLOBAL_CONFIG`] / [`set_pool`]), so render and
//! preview code reads them from here instead of storing clones on the apps.

pub mod embedding;
pub use embedding::*;

use sqlx::sqlite::SqlitePool;
use std::sync::OnceLock;

use crate::config::Config;

/// The config the TUI apps were built with: set once by
/// [`crate::ui::tasks::TasksApp::new`] and [`crate::ui::today::TodayApp::new`]
/// after the config has been `init_with` (color model built). Render and
/// preview code reads the current config from here via [`config`] instead
/// of storing a clone on the apps.
pub static GLOBAL_CONFIG: OnceLock<Config> = OnceLock::new();

/// The config stored by the TUI apps ([`GLOBAL_CONFIG`]). Panics when no
/// TUI app has been constructed yet — every UI path runs under one of the
/// apps, which store their config before use.
pub fn config() -> &'static Config {
    GLOBAL_CONFIG
        .get()
        .expect("GLOBAL_CONFIG: no TUI app has stored its config")
}

/// The app-wide database pool ([`set_pool`]).
///
/// In production the pool is reachable from any thread: the TUI spawns db
/// work onto the multi-thread tokio runtime (see `#[tokio::main]`), so a
/// thread-local would be empty on worker threads. Tests run single-threaded
/// (`#[tokio::test]` is current_thread), so they keep the pool in a
/// thread-local instead — each test's in-memory pool stays private and no
/// test can poison another's.
#[cfg(not(test))]
static GLOBAL_POOL: OnceLock<SqlitePool> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_POOL: std::cell::RefCell<Option<SqlitePool>> =
        const { std::cell::RefCell::new(None) };
}

/// Install the app-wide database pool. Called by `run_app` at startup and
/// by tests before constructing a TUI app. In production the first install
/// wins (a `OnceLock` cannot be replaced); a later install is logged and
/// ignored. In tests the thread-local is overwritten freely.
pub fn set_pool(pool: SqlitePool) {
    #[cfg(not(test))]
    if GLOBAL_POOL.set(pool).is_err() {
        log::warn!("GLOBAL_POOL: already set; keeping the first pool");
    }
    #[cfg(test)]
    TEST_POOL.with(|db| *db.borrow_mut() = Some(pool));
}

/// The app-wide database pool ([`set_pool`]). Returns a cheap clone (the
/// pool is an Arc handle). Panics when unset — every UI path runs under
/// `run_app` or an app constructor that installed it.
pub fn pool() -> SqlitePool {
    #[cfg(not(test))]
    {
        GLOBAL_POOL
            .get()
            .expect("GLOBAL_POOL: no pool installed (run_app sets it)")
            .clone()
    }
    #[cfg(test)]
    TEST_POOL.with(|db| {
        db.borrow()
            .clone()
            .expect("GLOBAL_POOL: no pool installed (tests must call set_pool)")
    })
}
