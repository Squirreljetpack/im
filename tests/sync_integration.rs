//! End-to-end sync tests against a local Miniflare server.
//!
//! `wrangler dev` (workerd + a local D1) is started once for the whole test
//! binary with a temp persistence directory; each test runs its own account and
//! its own pair of device databases, so the cases stay independent. Nothing
//! here touches Cloudflare: no account, no deployment.

use std::path::PathBuf;
use std::time::Duration;

use im::db::{TaskObject, create_task, test_pool};
use im::sync::client::Client;
use im::sync::session;
use im::sync::{KEY_AUTH_TOKEN, sync_once};
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
    session::state_set(&pool, KEY_AUTH_TOKEN, token)
        .await
        .expect("seed the token");
    pool
}

async fn sync(pool: &SqlitePool) -> session::SyncReport {
    sync_with(pool, &TrackerSlots::default()).await
}

async fn sync_with(pool: &SqlitePool, slots: &TrackerSlots) -> session::SyncReport {
    let server = server().await;
    sync_once(pool, &server.base, SYNC_TIMEOUT, slots)
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

/// Sync one device until the server has nothing more for it.
async fn drain(pool: &SqlitePool) {
    loop {
        let report = sync(pool).await;
        if !report.has_more {
            return;
        }
    }
}

/// Converge a set of devices: each round trip only learns what the *other*
/// device pushed before it, so a device needs one more round after a peer
/// pushes.
async fn settle(devices: &[&SqlitePool]) {
    for _ in 0..3 {
        for device in devices {
            drain(device).await;
        }
    }
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
