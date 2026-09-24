//! Coverage for the reminder delivery scheduler (gap T1b, issue #117).
//!
//! Exactly-once is the whole point, and both ways of getting it wrong are
//! silent. Delivering twice looks like a working scheduler to anyone not
//! counting; never delivering looks like a vault with nothing due. So the
//! tests drive `poll_once_with` through a counting hook rather than asserting
//! on log output.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::models::ReminderWindow;
use remind_me_core::reminders::{list_reminders, set_reminder};
use remind_me_core::scheduler::{due_reminders, poll_once_with};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::{params, Connection};

fn add(conn: &Connection, content: &str) -> String {
    remind_me_core::db::queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

fn past(hours: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
}

fn future(hours: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339()
}

/// Write `remind_at` directly. `set_reminder` refuses a past timestamp on
/// purpose, so a reminder that is already due can only be reached this way —
/// which in production means one that came due while nothing was running.
fn force_due(conn: &Connection, memory_id: &str, when: &str) {
    conn.execute(
        "UPDATE memories SET remind_at = ? WHERE id = ?",
        params![when, memory_id],
    )
    .unwrap();
}

/// Run a pass, returning the ids it delivered.
fn pass(conn: &Connection) -> Vec<String> {
    let mut delivered = Vec::new();
    poll_once_with(conn, &mut |m| delivered.push(m.id.clone())).unwrap();
    delivered
}

/// `POLL_INTERVAL_ENV` is process-wide, and two tests in this binary set it to
/// different values. Without this, one test's scheduler can read the other's
/// interval mid-run — which is how the loop test intermittently missed its
/// deadline under full-suite load while passing every time in isolation.
static POLL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_due_reminder_is_delivered_once_and_never_again() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "the thing you asked to be reminded of");
    force_due(&conn, &id, &past(1));

    assert_eq!(pass(&conn), vec![id.clone()]);

    // The second pass is the whole test. A scheduler that re-delivers looks
    // identical to a working one until you are the person being told.
    assert!(
        pass(&conn).is_empty(),
        "a delivered reminder must not fire again"
    );
}

#[test]
fn a_reminder_that_is_not_yet_due_is_left_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "later");
    set_reminder(&conn, &id, Some(&future(6))).unwrap();

    assert!(pass(&conn).is_empty());
    assert!(
        due_reminders(&conn).unwrap().is_empty(),
        "nothing is due yet"
    );
}

#[test]
fn a_reminder_that_came_due_while_nothing_was_running_is_delivered_on_the_next_pass() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "came due overnight");
    // Long past, as if the process had been down for days.
    force_due(&conn, &id, &past(72));

    // Confirmed against the reference rather than assumed: its due query has
    // no lower bound, so a late reminder is delivered rather than skipped.
    // Skipping would lose exactly the reminders a scheduler exists to catch.
    assert_eq!(pass(&conn), vec![id]);
}

#[test]
fn a_restart_does_not_re_deliver() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "survives a restart");
    force_due(&conn, &id, &past(2));
    assert_eq!(pass(&conn).len(), 1);

    // The delivery record lives in the database, not in the loop's memory, so
    // a fresh process reaches the same conclusion.
    let restarted = pass(&conn);
    assert!(restarted.is_empty(), "restart must not re-deliver");
}

#[test]
fn a_delivery_hook_that_panics_leaves_the_reminder_pending() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "delivery blew up");
    force_due(&conn, &id, &past(1));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let inner = Database::open_in_memory();
        drop(inner);
        poll_once_with(&conn, &mut |_| panic!("channel exploded")).unwrap()
    }));
    assert!(
        result.is_err(),
        "the panic propagates rather than being eaten"
    );

    // The delivery row is written *after* the hook, so a hook that dies
    // outright leaves the reminder to be tried again. Marked first, a crash
    // mid-delivery would silently consume the reminder.
    assert_eq!(
        pass(&conn),
        vec![id],
        "a reminder whose delivery died must still be pending"
    );
}

#[test]
fn a_deleted_memorys_reminder_is_never_delivered() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "deleted before it fired");
    force_due(&conn, &id, &past(1));
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = ?",
        params![chrono::Utc::now().to_rfc3339(), &id],
    )
    .unwrap();

    // Delivering this would surface content the user deleted, through a
    // channel they may not control.
    assert!(pass(&conn).is_empty());
}

#[test]
fn several_due_reminders_are_delivered_soonest_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let older = add(&conn, "due first");
    let newer = add(&conn, "due second");
    force_due(&conn, &older, &past(10));
    force_due(&conn, &newer, &past(1));

    assert_eq!(pass(&conn), vec![older, newer]);
}

#[test]
fn rescheduling_a_delivered_reminder_makes_it_deliverable_again() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "recurring chore");
    force_due(&conn, &id, &past(2));
    assert_eq!(pass(&conn).len(), 1);

    // Delivery is keyed on `(memory_id, remind_at)`. Keyed on the memory
    // alone, a chore could be reminded about exactly once, ever.
    force_due(&conn, &id, &past(1));
    assert_eq!(pass(&conn), vec![id]);
}

#[test]
fn a_delivered_reminder_drops_out_of_the_listing_too() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "handled");
    force_due(&conn, &id, &past(1));

    assert_eq!(
        list_reminders(&conn, ReminderWindow::Overdue, 20)
            .unwrap()
            .len(),
        1,
        "overdue before delivery"
    );
    pass(&conn);

    // The scheduler and `remind_me_list_reminders` read the same window, so a
    // reminder cannot sit visibly overdue in the tool while the loop considers
    // it handled.
    assert!(list_reminders(&conn, ReminderWindow::Overdue, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn a_second_poller_racing_the_first_does_not_error_the_pass() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "contended");
    let when = past(1);
    force_due(&conn, &id, &when);

    // Simulate the loser of the race: the delivery row already exists when
    // this pass tries to write it. `INSERT OR IGNORE` keeps the unique index
    // as the guarantee rather than letting it abort the whole pass — one
    // duplicate must not strand every later reminder in the same batch.
    let mut delivered = Vec::new();
    poll_once_with(&conn, &mut |m| {
        conn.execute(
            "INSERT INTO reminder_deliveries (memory_id, remind_at, delivered_at)
             VALUES (?, ?, ?)",
            params![m.id, when, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        delivered.push(m.id.clone());
    })
    .expect("a duplicate delivery row must not fail the pass");

    assert_eq!(delivered.len(), 1);
}

// ---------------------------------------------------------------------------
// The loop itself
// ---------------------------------------------------------------------------

/// A scratch directory that cleans itself up, following `backup_test.rs`.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "rmm_sched_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        );
        let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.0.join("remind_me.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_in_memory_database_gets_no_scheduler() {
    let db = Database::open_in_memory().unwrap();

    // The loop's thread opens its own connection by path, and `:memory:`
    // would hand it a different, empty database. Started anyway, it would
    // poll that empty database forever and look exactly like a vault with
    // nothing due.
    assert!(remind_me_core::scheduler::start_scheduler_for(&db.conn()).is_none());
}

#[test]
fn the_running_loop_delivers_without_anyone_calling_a_tool() {
    let dir = TempDir::new("loop");
    let path = dir.db_path();
    let id = {
        let db = Database::open(&path).unwrap();
        let conn = db.conn();
        let id = add(&conn, "fires on its own");
        force_due(&conn, &id, &past(1));
        id
    };

    // The point of the whole issue: a reminder fires because time passed, not
    // because something asked. Everything above this test drives `poll_once`
    // by hand, which would pass just as happily against a loop that never ran.
    let _env = POLL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(remind_me_core::scheduler::POLL_INTERVAL_ENV, "1");
    let scheduler = remind_me_core::scheduler::start_scheduler(path.clone());

    let observer = Database::open(&path).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut delivered = 0i64;
    while std::time::Instant::now() < deadline {
        delivered = observer
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM reminder_deliveries WHERE memory_id = ?",
                params![&id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if delivered > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    scheduler.stop();
    crate::test_env::remove_var(remind_me_core::scheduler::POLL_INTERVAL_ENV);

    assert_eq!(delivered, 1, "the loop delivered it unprompted");
}

#[test]
fn stopping_the_loop_does_not_wait_out_the_poll_interval() {
    let dir = TempDir::new("stop");
    let path = dir.db_path();
    drop(Database::open(&path).unwrap());

    // A long interval, so a `thread::sleep` loop would block shutdown for
    // most of it. Shutdown has to interrupt the wait, or stopping a server
    // stalls on a thread with nothing left to do.
    let _env = POLL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(remind_me_core::scheduler::POLL_INTERVAL_ENV, "3600");
    let scheduler = remind_me_core::scheduler::start_scheduler(path);
    let started = std::time::Instant::now();
    scheduler.stop();
    crate::test_env::remove_var(remind_me_core::scheduler::POLL_INTERVAL_ENV);

    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "stop took {:?}, so it waited on the interval rather than being woken",
        started.elapsed()
    );
}
