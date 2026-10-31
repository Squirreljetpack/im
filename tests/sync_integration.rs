//! End-to-end sync tests against a local Miniflare server.
//!
//! `wrangler dev` (workerd + a local D1) is started once for the whole test
//! binary with a temp persistence directory; each test runs its own account and
//! its own pair of device databases, so the cases stay independent. Nothing
//! here touches Cloudflare: no account, no deployment.

use std::path::PathBuf;
use std::time::Duration;

use im::db::{
    EntryObject, TaskObject, TrackerObject, TrackerValue, create_entry, create_task, test_pool,
};
use im::sync::client::Client;
use im::sync::session;
use im::sync::sync_once;
use im::tracker::TrackerSlots;
use sqlx::{Row, SqlitePool};
use tokio::sync::OnceCell;

/// The server startup budget: `wrangler dev` bundles and boots workerd.
const READY_ATTEMPTS: usize = 120;
const SYNC_TIMEOUT: Duration = Duration::from_secs(15);

struct Server {
    base: String,
    /// Kept alive for the whole test binary: dropping the handle would kill
    /// the server (`kill_on_drop`).
    _child: tokio::process::Child,
}

static SERVER: OnceCell<Server> = OnceCell::const_new();
static LOG: OnceCell<PathBuf> = OnceCell::const_new();

async fn server() -> &'static Server {
    SERVER.get_or_init(start_server).await
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("local addr").port()
}

async fn start_server() -> Server {
    let dir = std::env::temp_dir().join(format!("im-sync-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the persist dir");
    let persist = dir.to_string_lossy().to_string();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let migrations = tokio::process::Command::new("npx")
        .args([
            "wrangler",
            "d1",
            "migrations",
            "apply",
            "AUTH_DB",
            "--local",
            "--persist-to",
            &persist,
        ])
        .current_dir("sync-server")
        .output()
        .await
        .expect("run `wrangler d1 migrations apply`");
    assert!(
        migrations.status.success(),
        "d1 migrations failed:\n{}\n{}",
        String::from_utf8_lossy(&migrations.stdout),
        String::from_utf8_lossy(&migrations.stderr)
    );

    let log_path = dir.join("wrangler.log");
    let log = std::fs::File::create(&log_path).expect("create the wrangler log");
    let _ = LOG.set(log_path.clone());
    // `timeout` bounds the life of a test server that is never reaped.
    let child = tokio::process::Command::new("timeout")
        .args([
            "900",
            "npx",
            "wrangler",
            "dev",
            "--port",
            &port.to_string(),
            "--persist-to",
            &persist,
        ])
        .current_dir("sync-server")
        .stdout(log.try_clone().expect("clone the log handle"))
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn `wrangler dev`");

    wait_until_ready(&base).await;
    println!("sync-server ready on {base} (log: {})", log_path.display());
    Server {
        base,
        _child: child,
    }
}

/// Poll the server until it answers. A connection error means workerd is not
/// listening yet; any HTTP answer means it is.
async fn wait_until_ready(base: &str) {
    for attempt in 0..READY_ATTEMPTS {
        let probe_base = base.to_string();
        let probe = im::sync::client::run_blocking(move || {
            Client::new(&probe_base, None).register("probe@example.com", "correct horse battery")
        })
        .await;
        match probe {
            Ok(_) => return,
            Err(error) => {
                let message = format!("{error:#}");
                if !message.contains("Failed to reach") {
                    return;
                }
                if attempt % 20 == 19 {
                    println!("waiting for {base} ({message})");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let log = LOG
        .get()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    panic!("the sync server did not come up at {base}; wrangler log:\n{log}");
}

/// Register an account and sign a fresh device database in to it.
async fn device(name: &str) -> (SqlitePool, String) {
    let server = server().await;
    let email = format!("{name}@example.com");
    let account = Client::new(&server.base, None)
        .register(&email, "correct horse battery")
        .expect("register");
    let pool = test_pool().await.expect("in-memory db");
    session::store_account(&pool, &account)
        .await
        .expect("store the account");
    (pool, account.token)
}

/// A second device on the same account (what `im auth login` on another
/// machine produces).
async fn second_device(token: &str) -> SqlitePool {
    let pool = test_pool().await.expect("in-memory db");
    session::store_token(&pool, token)
        .await
        .expect("seed the token");
    pool
}

async fn sync(pool: &SqlitePool) -> session::SyncReport {
    sync_with(pool, &TrackerSlots::default()).await
}

/// One round trip that may fail, for the cases where failing is the answer.
async fn try_sync(pool: &SqlitePool) -> anyhow::Result<session::SyncReport> {
    let server = server().await;
    let mut pull = session::Pull::resume(pool).await?;
    sync_once(pool, &server.base, SYNC_TIMEOUT, &TrackerSlots::default(), &mut pull).await
}

async fn sync_with(pool: &SqlitePool, slots: &TrackerSlots) -> session::SyncReport {
    let server = server().await;
    let mut pull = session::Pull::resume(pool).await.expect("resume the pull");
    sync_once(pool, &server.base, SYNC_TIMEOUT, slots, &mut pull)
        .await
        .expect("sync")
}

fn task_object(name: &str, body: &str) -> TaskObject {
    TaskObject {
        id: None,
        short_id: None,
        name: name.to_string(),
        body: body.to_string(),
        priority: 5,
        start_time: Some(1_700_000_000),
        available_duration_secs: None,
        interval_secs: None,
        target_count: 0,
        optional: false,
        end_time: None,
        parent: None,
    }
}

async fn task_row(pool: &SqlitePool, name: &str) -> Option<(String, String, i64)> {
    sqlx::query("SELECT id, body, priority FROM todos WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await
        .expect("query the task")
        .map(|row| {
            (
                row.get::<String, _>("id"),
                row.get("body"),
                row.get("priority"),
            )
        })
}

async fn body_of(pool: &SqlitePool, name: &str) -> Option<String> {
    sqlx::query_scalar("SELECT body FROM todos WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await
        .expect("query the body")
}

/// Edit a task's body through the mutation the CLI uses, so the edit emits an
/// event like any other local change.
async fn set_body(pool: &SqlitePool, name: &str, body: &str) {
    let id = sqlx::query_scalar("SELECT id FROM todos WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("find the task");
    im::db::update_todo_body(pool, id, body)
        .await
        .expect("update the body");
}

/// Event timestamps have millisecond resolution, so two mutations meant to be
/// ordered have to be a tick apart for last-write-wins to see the order.
async fn sleep_between_mutations() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Converge a set of devices: each round trip only learns what the *other*
/// device pushed before it, so a device needs one more round after a peer
/// pushes.
async fn settle(devices: &[&SqlitePool]) {
    settle_with(devices, &TrackerSlots::default()).await
}

/// `settle` for a configuration whose tracker intervals matter (slot dedup).
async fn settle_with(devices: &[&SqlitePool], slots: &TrackerSlots) {
    for _ in 0..3 {
        for device in devices {
            loop {
                let report = sync_with(device, slots).await;
                if !report.has_more {
                    break;
                }
            }
        }
    }
}

/// A scheduled task: it has an availability window and no interval, which is
/// how the model tells it apart from the other two kinds.
fn scheduled_task_object(name: &str) -> TaskObject {
    TaskObject {
        available_duration_secs: Some(3600),
        ..task_object(name, "")
    }
}

/// A tracker entry with no mood, logged at `time` (the caller decides, so two
/// devices can be made to compete for one slot).
async fn log_tracker(pool: &SqlitePool, tracker_type: &str, value: i64, time: i64) {
    let entry = EntryObject {
        mood: String::new(),
        body: String::new(),
        time,
        embedding: None,
        score: None,
        trackers: vec![TrackerObject {
            tracker_type: tracker_type.to_string(),
            value: TrackerValue::Integer(value),
            replace_slot: None,
        }],
        duration: None,
        todo_id: None,
    };
    create_entry(pool, &entry).await.expect("log the entry");
}

#[tokio::test]
async fn auth_registers_logs_in_and_reports_status() {
    let server = server().await;
    let email = "accounts@example.com";
    let account = Client::new(&server.base, None)
        .register(email, "correct horse battery")
        .expect("register");
    assert_eq!(account.email, email);

    // A second device logging in gets its own token for the same account.
    let session = Client::new(&server.base, None)
        .login(email, "correct horse battery")
        .expect("login");
    assert_eq!(session.user_id, account.user_id);

    let status = Client::new(&server.base, Some(session.token))
        .status()
        .expect("status");
    assert_eq!(status.email, email);

    let wrong = Client::new(&server.base, None).login(email, "not the password");
    assert!(format!("{:#}", wrong.unwrap_err()).contains("invalid email or password"));
}

#[tokio::test]
async fn a_new_task_replicates_to_the_other_device() {
    let (device_a, token) = device("replicate").await;
    let (task, _) = create_task(&device_a, &task_object("replicated", ""))
        .await
        .unwrap();
    let report = sync(&device_a).await;
    assert!(report.pushed > 0, "the cells are pushed");
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let device_b = second_device(&token).await;
    let report = sync(&device_b).await;
    assert_eq!(report.created, 1, "the task arrives as one row");
    let row = task_row(&device_b, "replicated")
        .await
        .expect("task present");
    assert_eq!(row.0, task.to_string(), "the same entity id");
    let short_id: Option<i64> = sqlx::query_scalar("SELECT short_id FROM todos WHERE id = ?")
        .bind(task)
        .fetch_one(&device_b)
        .await
        .unwrap();
    assert_eq!(short_id, Some(1), "the projection is allocated locally");
}

#[tokio::test]
async fn a_field_takes_the_newest_write() {
    let (device_a, token) = device("converge").await;
    let device_b = second_device(&token).await;
    create_task(&device_a, &task_object("shared", "one")).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    // Two devices edit the same field while disconnected; B's stamp is later.
    set_body(&device_a, "shared", "from a").await;
    sleep_between_mutations().await;
    set_body(&device_b, "shared", "from b").await;

    settle(&[&device_a, &device_b]).await;

    assert_eq!(body_of(&device_a, "shared").await.as_deref(), Some("from b"));
    assert_eq!(body_of(&device_b, "shared").await.as_deref(), Some("from b"));
}

#[tokio::test]
async fn completions_from_two_devices_add_up() {
    let (device_a, token) = device("completions").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("counted", ""))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    im::db::update_task(&device_a, task, 2).await.unwrap();
    im::db::update_task(&device_b, task, 3).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    let total = |pool: SqlitePool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
        )
        .bind(task)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(total(device_a.clone()).await, 5);
    assert_eq!(total(device_b).await, 5);
}

#[tokio::test]
async fn an_undo_is_a_negative_delta_that_also_replicates() {
    let (device_a, token) = device("undo").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("undone", ""))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    im::db::update_task(&device_a, task, 3).await.unwrap();
    settle(&[&device_a, &device_b]).await;
    im::db::update_task(&device_b, task, -1).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todo_completions WHERE todo_id = ?")
        .bind(task)
        .fetch_one(&device_a)
        .await
        .unwrap();
    assert_eq!(rows, 2, "both facts are kept");
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
    )
    .bind(task)
    .fetch_one(&device_a)
    .await
    .unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn a_deletion_is_terminal_and_offers_a_copy() {
    let (device_a, token) = device("terminal").await;
    let device_b = second_device(&token).await;
    create_task(&device_a, &task_object("doomed", "important text"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    im::db::delete_task(&device_a, task_row(&device_a, "doomed").await.unwrap().0.parse().unwrap())
        .await
        .unwrap();
    sleep_between_mutations().await;
    // B has not heard about it yet, and edits the row.
    set_body(&device_b, "doomed", "still here").await;

    settle(&[&device_a, &device_b]).await;
    assert!(task_row(&device_a, "doomed").await.is_none());
    assert!(
        task_row(&device_b, "doomed").await.is_none(),
        "the edit cannot revive it"
    );
}

#[tokio::test]
async fn a_delete_strands_the_links_it_leaves() {
    let (device_a, token) = device("cascade").await;
    let device_b = second_device(&token).await;
    let (parent, _) = create_task(&device_a, &task_object("parent", ""))
        .await
        .unwrap();
    let mut child = task_object("child", "");
    child.parent = Some(parent);
    create_task(&device_a, &child).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    im::db::delete_task(&device_a, parent).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    let link: Option<String> = sqlx::query_scalar("SELECT parent FROM todos WHERE name = 'child'")
        .fetch_one(&device_b)
        .await
        .unwrap();
    assert_eq!(link, None, "the child is promoted to a root on every device");
}

#[tokio::test]
async fn a_fresh_database_rebuilds_itself_from_the_server() {
    let (device_a, token) = device("rebuild").await;
    let (task, _) = create_task(&device_a, &task_object("kept", "text"))
        .await
        .unwrap();
    sync(&device_a).await;

    let rebuilt = second_device(&token).await;
    sync(&rebuilt).await;
    assert_eq!(task_row(&rebuilt, "kept").await.unwrap().0, task.to_string());
    assert_eq!(body_of(&rebuilt, "kept").await.as_deref(), Some("text"));

    // A device that wipes its copy and pulls from cursor 0 gets the same rows.
    session::reset_local_state(&rebuilt).await.unwrap();
    sync(&rebuilt).await;
    assert_eq!(task_row(&rebuilt, "kept").await.unwrap().0, task.to_string());
}

#[tokio::test]
async fn a_scheduled_task_is_one_cell_across_devices() {
    let (device_a, token) = device("scheduled").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &scheduled_task_object("meditate"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    let count = |pool: SqlitePool| async move {
        let rows: Vec<(String, i32)> =
            sqlx::query_as("SELECT id, count FROM todo_completions WHERE todo_id = ?")
                .bind(task)
                .fetch_all(&pool)
                .await
                .unwrap();
        rows
    };

    im::db::set_scheduled_completion(&device_a, task, Some(1))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;
    let derived = im::db::scheduled_completion_id(task).to_string();
    assert_eq!(count(device_a.clone()).await, vec![(derived.clone(), 1)]);
    assert_eq!(
        count(device_b.clone()).await,
        vec![(derived.clone(), 1)],
        "the other device wrote the same derived id"
    );

    // Un-marking on B, which never saw the row's birthday, is the same cell.
    sleep_between_mutations().await;
    im::db::set_scheduled_completion(&device_b, task, None)
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;
    assert_eq!(count(device_a.clone()).await, vec![(derived.clone(), -1)]);
    assert_eq!(count(device_b.clone()).await, vec![(derived.clone(), -1)]);

    // And it can be marked again: un-marking spent nothing.
    sleep_between_mutations().await;
    im::db::set_scheduled_completion(&device_a, task, Some(0))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;
    assert_eq!(count(device_a.clone()).await, vec![(derived.clone(), 0)]);
    assert_eq!(count(device_b).await, vec![(derived, 0)]);
}

#[tokio::test]
async fn a_replace_mode_slot_keeps_one_entry_across_devices() {
    let (device_a, token) = device("slots").await;
    let device_b = second_device(&token).await;
    let config: im::config::Config = toml::from_str(
        r#"
[tracker.water]
kind = "integer"
interval = { anchor = "2026-01-01T00:00:00+00:00", span = "1 day" }
"#,
    )
    .expect("the tracker config parses");
    let slots = TrackerSlots::from_config(&config);

    // Both log into the same day slot while disconnected, so neither has seen
    // the other's row when it writes.
    let now = im::date::now();
    log_tracker(&device_a, "water", 1, now).await;
    log_tracker(&device_b, "water", 2, now + 1).await;
    settle_with(&[&device_a, &device_b], &slots).await;

    let entries = |pool: SqlitePool| async move {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT time, CAST(score AS TEXT) FROM tracker WHERE type = 'water'")
                .fetch_all(&pool)
                .await
                .unwrap();
        rows
    };
    // The later write owns the slot; the loser is deleted and the deletion is
    // published, so both devices agree on the same single row.
    assert_eq!(entries(device_a.clone()).await, vec![(now + 1, "2".into())]);
    assert_eq!(entries(device_b).await, vec![(now + 1, "2".into())]);
}

#[tokio::test]
async fn a_device_is_told_when_a_peer_replaces_its_cell() {
    let (device_a, token) = device("notices").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("shared", "mine"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    // B writes the same cell later, so A's already-pushed value is replaced.
    sleep_between_mutations().await;
    set_body(&device_b, "shared", "theirs").await;
    sync(&device_b).await;

    // A learns about it from the notice, not by watching the text change.
    let report = sync(&device_a).await;
    let notice = report
        .notices
        .iter()
        .find(|notice| notice.contains("body"))
        .unwrap_or_else(|| panic!("no notice in {:?}", report.notices));
    assert!(notice.contains("\"mine\""), "{notice}");
    assert!(notice.contains("\"theirs\""), "{notice}");
    assert_eq!(body_of(&device_a, "shared").await.as_deref(), Some("theirs"));

    // The notice is not repeated once the device has read past it.
    let report = sync(&device_a).await;
    assert!(
        !report.notices.iter().any(|notice| notice.contains("body")),
        "{:?}",
        report.notices
    );
    let _ = task;
}

/// A cursor is a position in one named log. Written by hand because no API can
/// produce a cursor that belongs elsewhere — that is the point of the test.
async fn forge_cursor(pool: &SqlitePool, value: &str) {
    sqlx::query(
        "INSERT INTO _sync_state (key, value) VALUES ('cursor', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(value)
    .execute(pool)
    .await
    .expect("forge the cursor");
}

/// A database that last synced somewhere else must not read this log through
/// its cursor: it would merge two unrelated histories, or — when its version is
/// past this log's end — silently do nothing at all, forever.
#[tokio::test]
async fn a_cursor_from_another_log_is_refused() {
    let (pool, _token) = device("foreign-cursor").await;
    let _ = create_task(&pool, &task_object("mine", "body"))
        .await
        .unwrap();
    let report = sync(&pool).await;
    assert!(report.created > 0 || report.applied > 0);
    let cursor = session::cursor(&pool).await.expect("read the cursor");
    let log = cursor.expect("the first sync names the log").log;

    forge_cursor(&pool, "another-log/0").await;
    let error = try_sync(&pool)
        .await
        .expect_err("a foreign cursor is refused");
    let message = format!("{error:#}");
    assert!(message.contains("different server log"), "{message}");
    assert!(message.contains(&log[..8]), "{message}");
    assert!(message.contains("im :sync --reset"), "{message}");

    // Nothing was applied, and the refusal is repeatable rather than a fluke.
    assert!(body_of(&pool, "mine").await.is_some());
    assert!(try_sync(&pool).await.is_err());
}

/// The same log, rewound by a restore: the versions this device consumed now
/// name other cells, so reading on from its cursor would skip or mis-file them.
#[tokio::test]
async fn a_cursor_past_the_end_of_the_log_is_refused() {
    let (pool, _token) = device("rewound").await;
    let _ = create_task(&pool, &task_object("mine", "body"))
        .await
        .unwrap();
    sync(&pool).await;
    let cursor = session::cursor(&pool)
        .await
        .expect("read the cursor")
        .expect("the first sync names the log");
    let log = cursor.log;

    forge_cursor(&pool, &format!("{log}/{}", cursor.version + 1000)).await;
    let error = try_sync(&pool)
        .await
        .expect_err("a cursor past the log is refused");
    let message = format!("{error:#}");
    assert!(message.contains("restored from an older copy"), "{message}");
    assert!(
        message.contains(&format!("ends at {}", cursor.version)),
        "{message}"
    );

    // Adopting the server's data is the repair, and it works.
    session::reset_local_state(&pool).await.expect("reset");
    try_sync(&pool).await.expect("rebuild from the server");
    assert!(body_of(&pool, "mine").await.is_some());
}
