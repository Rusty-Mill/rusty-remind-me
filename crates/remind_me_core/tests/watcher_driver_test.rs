//! The scan loop that actually drives the watcher (#203).
//!
//! `Watcher::scan_once` was implemented and covered by `watcher_test.rs`, and
//! nothing in the binary called it. Both status surfaces built a *fresh*
//! `Watcher::from_env()` and reported on an object that had never scanned, so
//! `scans` and every file counter were structurally zero while `enabled: true`
//! read exactly like a working feature.
//!
//! These tests drive the real loop against a real file on disk. A test that
//! only checked `start_watcher_for` returned `Some` would pass with a thread
//! that did nothing at all, which is the bug this replaces.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::watcher::{start_watcher_for, WatcherHandle, WATCH_DIRS_ENV};
use remind_me_core::Database;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `REMIND_ME_WATCH_DIRS` and the process-global live-watcher registry are
/// both shared, so these tests run one at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// A watch directory inside the default import root, matching
/// `watcher_test.rs` — a directory outside the roots is refused by design.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
            .join(format!("rrm_driver_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A database on disk. The loop opens its own connection by path, so an
/// in-memory database would give it a different, empty store.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rrm_driver_db_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("w.db"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

/// Poll `check` until it holds or the deadline passes.
///
/// Bounded, and it *fails* on timeout rather than hanging: a loop that never
/// scans must produce a failing assertion, not a CI timeout with nothing to
/// read.
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn memory_count(path: &std::path::Path) -> usize {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) as usize
}

/// Start a watcher over `dir` against `db_path`, with a 1-second interval.
fn start(dir: &std::path::Path, db_path: &std::path::Path) -> Option<WatcherHandle> {
    crate::test_env::set_var(WATCH_DIRS_ENV, dir.display().to_string());
    crate::test_env::set_var("REMIND_ME_WATCH_INTERVAL", "1");
    crate::test_env::set_var("REMIND_ME_WATCH_GRACE", "0");
    let db = Database::open(db_path).unwrap();
    // Bound rather than passed inline: `db.conn()` borrows `db`, and the
    // handle does not, so the connection has to be dropped before `db` goes
    // out of scope at the end of this function.
    let conn = db.conn();
    let handle = start_watcher_for(&conn);
    drop(conn);
    handle
}

fn clear_env() {
    crate::test_env::remove_var(WATCH_DIRS_ENV);
    crate::test_env::remove_var("REMIND_ME_WATCH_INTERVAL");
    crate::test_env::remove_var("REMIND_ME_WATCH_GRACE");
}

#[test]
fn the_loop_ingests_a_file_without_anyone_calling_scan_once() {
    // The whole point of #203. Nothing here touches `scan_once`; if the loop
    // does not run, no memory appears and this fails.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("ingest");
    let db = TempDb::new("ingest");
    std::fs::write(
        scratch.0.join("note.md"),
        "# A watched note\n\nSomething worth remembering about quokkas.\n",
    )
    .unwrap();

    let handle = start(&scratch.0, &db.0).expect("watcher should start");

    wait_for("the watched file to be ingested", || {
        memory_count(&db.0) > 0
    });

    handle.stop();
    clear_env();
}

#[test]
fn status_reports_the_running_loop_rather_than_a_fresh_watcher() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("status");
    let db = TempDb::new("status");
    std::fs::write(scratch.0.join("note.md"), "# Note\n\nBody text here.\n").unwrap();

    let handle = start(&scratch.0, &db.0).expect("watcher should start");

    // `running` is the field that had no honest value before this.
    wait_for("the loop to report itself running", || {
        remind_me_core::watcher::live_status().is_some_and(|s| s.running)
    });
    // And the counters must move, which is what proves the status is reading
    // the live watcher rather than a freshly built one — a fresh `Watcher`
    // always reports `scans: 0`.
    wait_for("the scan counter to advance", || {
        remind_me_core::watcher::live_status().is_some_and(|s| s.scans > 0)
    });

    let status = remind_me_core::watcher::live_status().unwrap();
    assert!(status.enabled, "a running watcher is also enabled");
    assert!(status.running);
    assert!(status.scans > 0, "scans: {}", status.scans);

    handle.stop();
    clear_env();
}

#[test]
fn stopping_the_watcher_clears_the_running_status() {
    // A `running: true` outliving its thread would be the same misreport in a
    // new place.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("stop");
    let db = TempDb::new("stop");

    let handle = start(&scratch.0, &db.0).expect("watcher should start");
    wait_for("the loop to register itself", || {
        remind_me_core::watcher::live_status().is_some()
    });

    handle.stop();

    assert!(
        remind_me_core::watcher::live_status().is_none(),
        "a stopped watcher must not still report itself running"
    );
    clear_env();
}

#[test]
fn no_watch_dirs_means_no_loop() {
    // The watcher has an explicit enable switch, unlike the scheduler. Running
    // a thread that scans nothing would burn a wakeup a second for no reason.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let db = TempDb::new("nodirs");
    let database = Database::open(&db.0).unwrap();

    assert!(
        start_watcher_for(&database.conn()).is_none(),
        "nothing configured, so nothing to run"
    );
    assert!(remind_me_core::watcher::live_status().is_none());
}

#[test]
fn an_in_memory_database_does_not_start_a_loop() {
    // The loop opens its own connection by path, so `:memory:` would give it a
    // different, empty database and it would ingest into a store nobody can
    // read. Same reason the scheduler refuses.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("inmem");
    crate::test_env::set_var(WATCH_DIRS_ENV, scratch.0.display().to_string());

    let db = Database::open_in_memory().unwrap();
    assert!(
        start_watcher_for(&db.conn()).is_none(),
        "an in-memory database has no path for the loop's own connection"
    );

    clear_env();
}
