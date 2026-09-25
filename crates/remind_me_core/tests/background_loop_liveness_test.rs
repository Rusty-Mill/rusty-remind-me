//! Coverage for #270: the reminder scheduler and the promotion-backlog
//! nudge loop now report whether their thread is actually running, the
//! same way `watcher_driver_test.rs` already proved for the folder watcher.
//!
//! Neither loop has a way to fail mid-run that a test can trigger honestly
//! (no reachable panic path), so what these tests prove is the wiring: a
//! started loop reports `running: true`, and `stop()` correctly clears it
//! back to `false`/`None`. The underlying mechanism that also catches a
//! genuine panic — `scheduler::Liveness`/`LivenessGuard` — has its own
//! direct unit tests in `scheduler.rs`, including one that forces an actual
//! unwind through a held guard.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::Database;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The process-global live-scheduler/live-nudge registries are shared, so
/// these tests run one at a time — same reasoning `watcher_driver_test.rs`
/// gives for its own `LOCK`.
static LOCK: Mutex<()> = Mutex::new(());

/// A database on disk. Both loops open their own connection by path, so an
/// in-memory database would give them a different, empty store.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rrm_loop_liveness_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("db.sqlite"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

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

#[test]
fn a_started_scheduler_reports_running_and_stopping_it_clears_that() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = TempDb::new("scheduler");
    let database = Database::open(&db.0).unwrap();
    let handle = remind_me_core::scheduler::start_scheduler_for(&database.conn())
        .expect("scheduler always starts against a file-backed database");

    wait_for("the scheduler to report itself running", || {
        remind_me_core::scheduler::live_status().running
    });
    let status = remind_me_core::scheduler::live_status();
    assert!(status.running);
    assert!(status.poll_interval_seconds > 0);

    handle.stop();

    assert!(
        !remind_me_core::scheduler::live_status().running,
        "a stopped scheduler must not still report itself running"
    );
}

#[test]
fn a_started_nudge_loop_reports_running_and_stopping_it_clears_that() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV, "1");
    let db = TempDb::new("nudge");
    let database = Database::open(&db.0).unwrap();
    let handle = remind_me_core::promotion::start_nudge_for(&database.conn())
        .expect("nudge starts once an interval is configured against a file-backed database");

    wait_for("the nudge loop to report itself running", || {
        remind_me_core::promotion::nudge_running()
    });
    assert!(remind_me_core::promotion::nudge_running());

    handle.stop();

    assert!(
        !remind_me_core::promotion::nudge_running(),
        "a stopped nudge loop must not still report itself running"
    );
    crate::test_env::remove_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV);
}

#[test]
fn nudge_running_is_false_with_no_interval_configured() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV);
    assert!(
        !remind_me_core::promotion::nudge_running(),
        "nothing configured, so nothing started, so nothing running"
    );
}
