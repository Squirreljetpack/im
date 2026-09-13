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
use im::sync::{ConflictKind, KEY_AUTH_TOKEN, sync_once};
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
    assert_eq!(report.pushed, 1, "the insert is pushed");
    assert!(report.settled());

    let device_b = second_device(&token).await;
    let report = sync(&device_b).await;
    assert_eq!(report.applied, 1, "the task arrives");
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
async fn concurrent_edits_converge_on_both_devices() {
    let (device_a, token) = device("converge").await;
    let device_b = second_device(&token).await;
    create_task(&device_a, &task_object("shared", "initial"))
        .await
        .unwrap();
    sync(&device_a).await;
    sync(&device_b).await;
    assert_eq!(
        body_of(&device_b, "shared").await.as_deref(),
        Some("initial")
    );

    // Both devices edit while they cannot see each other.
    set_body(&device_a, "shared", "from A").await;
    set_body(&device_b, "shared", "from B").await;
    settle(&[&device_a, &device_b]).await;
    // Whichever edit wins, both devices must agree.
    let a = body_of(&device_a, "shared").await;
    let b = body_of(&device_b, "shared").await;
    assert_eq!(a, b, "last-write-wins must converge (A={a:?}, B={b:?})");

    // An edit made after seeing the winner always wins: B now knows both
    // events, so its next edit is stamped above them.
    set_body(&device_b, "shared", "newest from B").await;
    let device_c = second_device(&token).await;
    settle(&[&device_a, &device_b, &device_c]).await;
    assert_eq!(
        body_of(&device_b, "shared").await.as_deref(),
        Some("newest from B")
    );
    assert_eq!(
        body_of(&device_a, "shared").await.as_deref(),
        Some("newest from B")
    );
}

#[tokio::test]
async fn completions_from_two_devices_add_up() {
    let (device_a, token) = device("complete").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("pushups", ""))
        .await
        .unwrap();
    sync(&device_a).await;
    sync(&device_b).await;

    im::db::update_task(&device_a, task, 2).await.unwrap();
    im::db::update_task(&device_b, task, 3).await.unwrap();
    settle(&[&device_a, &device_b]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b)] {
        let total: i32 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
        )
        .bind(task)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            total, 5,
            "device {label} must see both devices' completions"
        );
    }
}

/// A deletion wins over an edit made after it, on the device that made the
/// edit too: no prompt, nothing to decide (§4.2.2).
#[tokio::test]
async fn a_deletion_wins_over_a_later_local_edit() {
    let (device_a, token) = device("outlive").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("kept", "original"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    // A deletes first, then B edits: the edit is the later decision by
    // timestamp, and still loses.
    im::db::delete_task(&device_a, task).await.unwrap();
    sleep_between_mutations().await;
    set_body(&device_b, "kept", "still here").await;
    sync(&device_a).await;

    let report = sync(&device_b).await;
    assert!(
        report.conflicts.is_empty(),
        "a deletion is applied without asking"
    );
    assert!(report.applied >= 1, "the deletion lands on B");
    assert!(
        task_row(&device_b, "kept").await.is_none(),
        "B drops the task it had just edited"
    );

    // The losing edit reaches A and is discarded there, not resurrected.
    let report = sync(&device_a).await;
    assert!(report.discarded >= 1, "the later edit is discarded");
    settle(&[&device_a, &device_b]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b)] {
        assert!(
            task_row(pool, "kept").await.is_none(),
            "device {label} must end up without the deleted task"
        );
    }
}

/// An edit this device has not pushed yet is discarded by the deletion too:
/// there is no conflict to confirm.
#[tokio::test]
async fn a_deletion_discards_an_unsynced_edit() {
    let (device_a, token) = device("delete").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("doomed", ""))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    set_body(&device_b, "doomed", "still here").await;
    sleep_between_mutations().await;
    im::db::delete_task(&device_a, task).await.unwrap();
    sync(&device_a).await;

    let report = sync(&device_b).await;
    assert!(
        report.conflicts.is_empty(),
        "an unsynced edit is discarded, not confirmed"
    );
    settle(&[&device_a, &device_b]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b)] {
        assert!(
            task_row(pool, "doomed").await.is_none(),
            "device {label} must honour the deletion"
        );
    }
}

/// The rule holds across three devices: the one that never saw either decision
/// converges on the deletion, and nothing resurfaces an edit made after it.
#[tokio::test]
async fn a_deletion_holds_across_three_devices() {
    let (device_a, token) = device("triple").await;
    let device_b = second_device(&token).await;
    let device_c = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("shared", "original"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b, &device_c]).await;

    im::db::delete_task(&device_a, task).await.unwrap();
    sleep_between_mutations().await;
    set_body(&device_b, "shared", "edited after the delete").await;
    settle(&[&device_a, &device_b, &device_c]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b), ("C", &device_c)] {
        assert!(
            task_row(pool, "shared").await.is_none(),
            "device {label} must converge on the deletion"
        );
    }
}

#[tokio::test]
async fn a_retried_push_does_not_duplicate_events() {
    let (device_a, _) = device("retry").await;
    create_task(&device_a, &task_object("once", ""))
        .await
        .unwrap();
    let first = sync(&device_a).await;
    assert_eq!(first.pushed, 1);

    // Simulate a lost acknowledgement: the outbox is re-pushed.
    sqlx::query("UPDATE _sync_events SET synced = 0")
        .execute(&device_a)
        .await
        .unwrap();
    let second = sync(&device_a).await;
    assert_eq!(second.pushed, 1, "the event is retried");
    assert_eq!(
        second.server_version, first.server_version,
        "the server must not append it twice"
    );
}

#[tokio::test]
async fn an_unreachable_server_never_stalls_the_cli() {
    let pool = test_pool().await.unwrap();
    // A port nothing listens on, plus a token so the sync is attempted.
    let dead = format!("http://127.0.0.1:{}", free_port());
    session::state_set(&pool, KEY_AUTH_TOKEN, "not-a-real-token")
        .await
        .unwrap();
    create_task(&pool, &task_object("queued", ""))
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let failed = sync_once(
        &pool,
        &dead,
        Duration::from_secs(1),
        &TrackerSlots::default(),
    )
    .await;
    assert!(failed.is_err(), "the sync must fail");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a dead server must not hold the command"
    );
    assert_eq!(
        im::sync::apply::pending_events(&pool).await.unwrap().len(),
        1,
        "the event stays queued"
    );
}

/// A backlog deeper than one server page arrives page by page, the way
/// `im sync` loops it (`@@SYNC.md` §5.1, §6.9).
#[tokio::test]
async fn a_deep_backlog_is_pulled_page_by_page() {
    let (device_a, token) = device("paging").await;
    let (task, _) = create_task(&device_a, &task_object("counted", ""))
        .await
        .unwrap();
    for _ in 0..1001 {
        im::db::update_task(&device_a, task, 1).await.unwrap();
    }
    let pushed = sync(&device_a).await;
    assert_eq!(pushed.pushed, 1002, "the task and its completions go out");

    let device_b = second_device(&token).await;
    let mut arrived = 0;
    let mut pages = 0;
    loop {
        let report = sync(&device_b).await;
        arrived += report.applied + report.stale;
        pages += 1;
        if !report.has_more {
            assert_eq!(report.server_version, 1002, "the cursor reaches the head");
            break;
        }
        assert!(pages < 5, "the backlog is barely over one page");
    }
    assert_eq!(arrived, 1002, "every event arrived exactly once");
    assert_eq!(pages, 2, "a full page plus the tail");
    let total: i32 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(count), 0) FROM todo_completions WHERE todo_id = ?",
    )
    .bind(task)
    .fetch_one(&device_b)
    .await
    .unwrap();
    assert_eq!(total, 1001, "the completions all landed");
}

/// Edit one task field through the mutation the CLI uses (`edit_task` builds a
/// field-level diff from what actually changed).
async fn edit_task_field(
    pool: &SqlitePool,
    id: im::db::Id,
    change: impl FnOnce(&mut im::db::UpdateTaskObject),
) {
    let row = im::db::fetch_task_by_id(pool, id, im::date::now())
        .await
        .expect("read the task")
        .expect("the task exists");
    let mut update = im::db::UpdateTaskObject {
        id,
        short_id: row.short_id,
        name: row.name.clone(),
        body: row.body.clone(),
        priority: row.priority,
        start_time: row.start_time,
        available_duration_secs: row.available_duration_secs,
        interval_secs: row.interval_secs,
        target_count: row.target_count,
        optional: row.optional != 0,
        end_time: row.end_time,
        parent: row.parent,
    };
    change(&mut update);
    im::db::edit_task(pool, &update)
        .await
        .expect("edit the task");
}

async fn task_column(pool: &SqlitePool, id: im::db::Id, column: &str) -> Option<i64> {
    let sql = format!("SELECT {column} FROM todos WHERE id = ?");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read the column")
}

/// Two devices editing *different* fields of one task keep both edits, however
/// the events order on the server (§4.1.1) — the property that rules out a
/// per-entity watermark.
#[tokio::test]
async fn disjoint_edits_from_two_devices_both_survive() {
    let (device_a, token) = device("fields").await;
    let device_b = second_device(&token).await;
    let (task, _) = create_task(&device_a, &task_object("shared", "note"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    edit_task_field(&device_a, task, |update| update.priority = 9).await;
    edit_task_field(&device_b, task, |update| {
        update.start_time = Some(1_800_000_000)
    })
    .await;
    settle(&[&device_a, &device_b]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b)] {
        assert_eq!(
            task_column(pool, task, "priority").await,
            Some(9),
            "device {label} keeps the priority edit"
        );
        assert_eq!(
            task_column(pool, task, "start_time").await,
            Some(1_800_000_000),
            "device {label} keeps the start-time edit"
        );
    }
}

/// Both devices replacing a note is the promptable text conflict of §4.2.1,
/// and appending both converges on every device.
#[tokio::test]
async fn a_contested_note_is_settled_by_the_user() {
    let (device_a, token) = device("note").await;
    let device_b = second_device(&token).await;
    let (_task, _) = create_task(&device_a, &task_object("shared", "base"))
        .await
        .unwrap();
    settle(&[&device_a, &device_b]).await;

    set_body(&device_a, "shared", "from A").await;
    set_body(&device_b, "shared", "from B").await;
    sync(&device_a).await;

    let report = sync(&device_b).await;
    assert_eq!(report.conflicts.len(), 1, "one note conflict");
    assert_eq!(report.conflicts[0].kind, ConflictKind::TextReplaced);
    assert_eq!(
        report.conflicts[0].remote_value.as_deref(),
        Some("from A"),
        "the incoming note is the one from A"
    );
    session::resolve(
        &device_b,
        &report.conflicts[0],
        im::sync::Resolution::AppendBoth,
    )
    .await
    .expect("resolve the note conflict");
    settle(&[&device_a, &device_b]).await;

    for (label, pool) in [("A", &device_a), ("B", &device_b)] {
        assert_eq!(
            body_of(pool, "shared").await.as_deref(),
            Some("from B\nfrom A"),
            "device {label} keeps both notes"
        );
    }
}

/// A device that rebuilds its local database from the log gets every entity
/// back — including the ones it authored itself, which the server never echoes
/// — and asks nothing (§4.4.3). Its own unsynced work is gone, the server's
/// state is not.
#[tokio::test]
async fn a_reset_rebuilds_local_state_from_the_log() {
    let (device_a, token) = device("reset").await;
    let device_b = second_device(&token).await;

    create_task(&device_a, &task_object("alpha", "from A"))
        .await
        .expect("create the task on A");
    sync(&device_a).await;
    drain(&device_b).await;

    // B authors an event of its own, then diverges with one that never left
    // the device.
    create_task(&device_b, &task_object("beta", "from B"))
        .await
        .expect("create the task on B");
    sync(&device_b).await;
    sleep_between_mutations().await;
    create_task(&device_b, &task_object("local only", "never synced"))
        .await
        .expect("create the divergent task");
    assert_eq!(
        session::unsynced_count(&device_b).await.unwrap(),
        1,
        "the divergent task is still queued"
    );

    // The reset drops it and pulls the log from version 0 in one pass.
    assert_eq!(session::reset_local_state(&device_b).await.unwrap(), 1);
    assert!(
        task_row(&device_b, "beta").await.is_none(),
        "the local database is empty after the reset"
    );

    let report = sync(&device_b).await;
    assert!(
        report.conflicts.is_empty(),
        "a rebuild meets no conflicts: nothing local is left to contradict"
    );
    assert!(report.applied >= 2, "the log is replayed from version 0");
    assert_eq!(
        body_of(&device_b, "alpha").await.as_deref(),
        Some("from A"),
        "the task A wrote is back"
    );
    assert_eq!(
        body_of(&device_b, "beta").await.as_deref(),
        Some("from B"),
        "the task this device wrote before the reset is back too"
    );
    assert!(
        task_row(&device_b, "local only").await.is_none(),
        "the discarded task does not come back"
    );

    // The rebuilt watermarks are real: a later edit converges as usual.
    sleep_between_mutations().await;
    set_body(&device_a, "alpha", "edited after the reset").await;
    sync(&device_a).await;
    sync(&device_b).await;
    assert_eq!(
        body_of(&device_b, "alpha").await.as_deref(),
        Some("edited after the reset")
    );
}

/// The reset also drops the device id: the server skips the events a device
/// authored, so a rebuilt device has to come back as a new one.
#[tokio::test]
async fn a_reset_rotates_the_device_id() {
    let (device_a, _token) = device("rotate").await;
    create_task(&device_a, &task_object("alpha", "body"))
        .await
        .expect("create the task");
    sync(&device_a).await;
    let before = session::device_id_of(&mut device_a.acquire().await.unwrap())
        .await
        .unwrap();

    session::reset_local_state(&device_a).await.unwrap();
    let after = session::device_id_of(&mut device_a.acquire().await.unwrap())
        .await
        .unwrap();
    assert_ne!(before, after, "the rebuilt device has a fresh identity");

    // And it is still a working peer: its own old event is replayed.
    let report = sync(&device_a).await;
    assert!(report.applied >= 1);
    assert_eq!(
        body_of(&device_a, "alpha").await.as_deref(),
        Some("body"),
        "the log survives the identity change"
    );
}

/// Log one non-cumulative tracker entry, as the CLI does for a configured type.
async fn log_sleep(pool: &SqlitePool, time: i64, score: i32) {
    use im::db::{EntryObject, TrackerObject, TrackerValue};
    im::db::create_entry(
        pool,
        &EntryObject {
            mood: String::new(),
            body: String::new(),
            time,
            embedding: None,
            score: None,
            trackers: vec![TrackerObject {
                tracker_type: "sleep".to_string(),
                value: TrackerValue::Integer(score.into()),
                replace_slot: Some((time, time + 3_600)),
            }],
            duration: None,
            todo_id: None,
        },
    )
    .await
    .expect("log a tracker entry");
}

/// Non-cumulative slot rules for `sleep`, as the CLI builds them from config.
fn sleep_slots() -> TrackerSlots {
    use im::config::{Config, TrackerInterval, TrackerKind, TrackerSetting};
    let mut config = Config::default();
    config.tracker.insert(
        "sleep".to_string(),
        TrackerSetting::new(TrackerKind::Integer).with_interval(TrackerInterval {
            anchor: 0,
            span: jiff::Span::new().seconds(3_600),
            cumulative: false,
        }),
    );
    TrackerSlots::from_config(&config)
}

async fn sleep_rows(pool: &SqlitePool) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT CAST(score AS INTEGER) FROM tracker WHERE type = 'sleep' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("read the sleep entries")
}

/// Two devices logging the same non-cumulative slot converge on one entry: the
/// loser is deleted locally, its deletion is pushed in the same `sync_once`
/// call (§4.1.4), and a peer that never evaluated the slot still honours it.
#[tokio::test]
async fn a_tracker_slot_loser_is_deleted_and_flushed() {
    let slots = sleep_slots();
    let (device_a, token) = device("slots").await;
    let device_b = second_device(&token).await;
    let time = 1_700_000_000;

    log_sleep(&device_a, time, 4).await;
    sync_with(&device_a, &slots).await;
    sleep_between_mutations().await;
    log_sleep(&device_b, time, 7).await;

    // B pulls A's entry into the slot its own entry already occupies: B's later
    // entry wins, and the tombstone of the loser leaves the outbox here.
    let report = sync_with(&device_b, &slots).await;
    assert!(
        report.pushed >= 2,
        "the loser's deletion is pushed in the same sync, got {}",
        report.pushed
    );
    assert_eq!(sleep_rows(&device_b).await, vec![7]);

    // A converges on B's entry through the tombstone, and the duplicate
    // tombstone A emits for its own row is a no-op for everyone.
    let report = sync_with(&device_a, &slots).await;
    assert!(report.conflicts.is_empty(), "slot repair asks nothing");
    assert_eq!(sleep_rows(&device_a).await, vec![7]);

    settle(&[&device_a, &device_b]).await;
    assert_eq!(sleep_rows(&device_a).await, vec![7], "no ping-pong on A");
    assert_eq!(sleep_rows(&device_b).await, vec![7], "no ping-pong on B");
}
