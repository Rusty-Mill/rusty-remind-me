//! The background loop that delivers a due reminder, exactly once.
//!
//! # Why a deliveries table and not "`remind_at` is in the past"
//!
//! A bare past-timestamp check has no way to remember that it already fired,
//! so it leaves only two behaviours, both wrong: deliver on every pass forever,
//! or skip anything already past and lose it. Recording each delivery in
//! `reminder_deliveries` — keyed `(memory_id, remind_at)`, uniquely indexed —
//! is what makes the third behaviour possible, which is the one people expect:
//! a reminder that came due while nothing was running is delivered once on the
//! next pass after start, and then never again.
//!
//! That is confirmed against the reference rather than assumed: its `poll_once`
//! selects `remind_at <= now` with no lower bound, so it delivers late rather
//! than skipping.
//!
//! # A failed channel does not hold the reminder back
//!
//! Delivery is a log line *plus* a best-effort fan-out to whatever
//! notification channels are configured. The delivery row is written either
//! way, so a webhook that is down does not cause the same reminder to be
//! re-attempted on the next pass. The reference makes the same call, and the
//! reason holds up: the log line is the channel that cannot fail, so the
//! reminder has been delivered somewhere. Retrying instead would mean
//! re-logging the same reminder every 60 seconds for as long as the webhook
//! stayed down — turning one missed notification into an unbounded stream of
//! duplicates through the channel that *was* working.
//!
//! # Nothing configured still delivers
//!
//! With no channel set up, [`poll_once`] still logs and still records the
//! delivery. "Channels are opt-in" governs whether a *notification* is
//! attempted, not whether the reminder is considered handled — a vault with no
//! webhook must not silently accumulate undelivered reminders forever.

use crate::models::Memory;
use crate::notifications;
use crate::reminders;
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const POLL_INTERVAL_ENV: &str = "REMIND_ME_REMINDER_POLL_INTERVAL";

/// Seconds between passes. The scheduler always runs; this only tunes how
/// often it looks, not whether it is enabled.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 60;

/// How much of a memory's content a delivery line carries. Enough to
/// recognise which reminder fired without pasting a whole document into a log.
const PREVIEW_CHARS: usize = 200;

pub fn configured_poll_interval() -> Duration {
    Duration::from_secs(
        std::env::var(POLL_INTERVAL_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
    )
}

fn preview(content: &str) -> String {
    let mut out: String = content.chars().take(PREVIEW_CHARS).collect();
    if content.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Reminders that are due and not yet delivered.
///
/// This is `reminders::list_reminders`' `Overdue` window with no limit, and
/// deliberately the same definition: the scheduler deciding "due" differently
/// from what `remind_me_list_reminders` shows would mean a reminder could sit
/// visibly overdue while the loop never picked it up.
pub fn due_reminders(conn: &Connection) -> Result<Vec<Memory>> {
    reminders::list_reminders(conn, crate::models::ReminderWindow::Overdue, i64::MAX)
}

/// Log the reminder, then fan out to any configured channel.
///
/// Logging is unconditional and happens first, so it survives a channel that
/// is misconfigured, slow, or absent.
pub fn deliver(memory: &Memory) {
    eprintln!(
        "reminder due — memory `{}` (remind_at={}): {}",
        memory.id,
        memory.remind_at.as_deref().unwrap_or("?"),
        preview(&memory.content)
    );
    notifications::notify(
        &format!("Reminder due: memory `{}`", memory.id),
        &memory.content,
    );
}

/// Deliver every due, not-yet-delivered reminder once. Returns how many.
pub fn poll_once(conn: &Connection) -> Result<usize> {
    poll_once_with(conn, &mut deliver)
}

/// [`poll_once`] with the delivery step injected.
///
/// The seam exists so a test can assert *which* reminders a pass picks up
/// without a live webhook, and so an embedder can replace delivery wholesale
/// without touching the due query.
pub fn poll_once_with(conn: &Connection, deliver: &mut dyn FnMut(&Memory)) -> Result<usize> {
    let due = due_reminders(conn)?;
    let mut delivered = 0usize;

    for memory in &due {
        let Some(remind_at) = memory.remind_at.as_deref() else {
            continue;
        };
        deliver(memory);
        // Written after the delivery attempt, not before: a panic in a
        // delivery hook should leave the reminder pending rather than mark it
        // handled. `INSERT OR IGNORE` because the unique index is the real
        // guarantee — two racing pollers must produce one delivery, not an
        // error that aborts the pass.
        conn.execute(
            "INSERT OR IGNORE INTO reminder_deliveries (memory_id, remind_at, delivered_at)
             VALUES (?, ?, ?)",
            params![memory.id, remind_at, chrono::Utc::now().to_rfc3339()],
        )?;
        delivered += 1;
    }

    Ok(delivered)
}

// ---------------------------------------------------------------------------
// Thread lifecycle
// ---------------------------------------------------------------------------

/// Shared stop flag plus the condvar the loop sleeps on.
///
/// A condvar rather than `thread::sleep`: shutdown has to interrupt the wait,
/// or stopping the server would block for up to a full poll interval on a
/// thread that has nothing left to do.
/// Shared with [`crate::watcher`], which needs the same "sleep for an interval
/// but wake immediately on shutdown" primitive. Kept here, where it was first
/// written, rather than moved to a new module for two users.
pub(crate) struct Stop {
    stopped: AtomicBool,
    waker: Mutex<()>,
    condvar: Condvar,
}

impl Stop {
    pub(crate) fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            waker: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _guard = self.waker.lock().unwrap();
        self.condvar.notify_all();
    }

    /// Sleep for `interval`, waking early if stopped.
    pub(crate) fn wait(&self, interval: Duration) {
        let guard = self.waker.lock().unwrap();
        if self.is_stopped() {
            return;
        }
        let _unused = self.condvar.wait_timeout(guard, interval).unwrap();
    }
}

/// Cheap, shared proof that a background loop's thread has not exited — by
/// returning normally or by panicking — since it started.
///
/// A `std::thread::JoinHandle` answers the same question via
/// [`std::thread::JoinHandle::is_finished`], but can only be owned once; a
/// status surface and whichever handle a caller stops the loop with are two
/// separate readers that each need their own way to ask. Cloning a
/// `Liveness` gives every reader that without needing to share the
/// `JoinHandle` itself.
///
/// Shared with [`crate::watcher`] and [`crate::promotion`]'s nudge loop —
/// three background loops, the same "is anyone actually watching" question.
/// Kept here with [`Stop`], for the same reason that primitive is: written
/// first here, reused rather than duplicated or relocated for three users.
#[derive(Clone)]
pub(crate) struct Liveness(Arc<AtomicBool>);

impl Liveness {
    /// A fresh `Liveness`, `true` until its paired [`LivenessGuard`] drops.
    pub(crate) fn new() -> (Self, LivenessGuard) {
        let flag = Arc::new(AtomicBool::new(true));
        (Self(Arc::clone(&flag)), LivenessGuard(flag))
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Marks its [`Liveness`] false when dropped — including while unwinding
/// from a panic, which is exactly the case a liveness signal exists to
/// catch: a background loop dying without ever calling its own `stop()`
/// must still be observable as not-running, not silently misreported as
/// healthy forever because nothing else changed the flag.
///
/// A loop's thread holds this for its entire run — bound near the top of
/// the spawned closure, before its own `while` loop — so the flag only
/// flips once that stack frame is actually gone, panic or ordinary return
/// alike.
pub(crate) struct LivenessGuard(Arc<AtomicBool>);

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The reminder scheduler's state, for `remind_me_server_status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerStatus {
    /// Whether the poll loop's thread is actually running right now.
    ///
    /// `false` with the rest of the process otherwise healthy means the
    /// thread panicked, or a scheduler was never started for this database
    /// (an in-memory one, or a process that never called
    /// [`start_scheduler_for`]) — distinct failure modes a bare "is the
    /// scheduler configured" check cannot tell apart, since the scheduler
    /// has no configuration to be missing in the first place (#270).
    pub running: bool,
    pub poll_interval_seconds: u64,
}

/// The scheduler this process is running, if any.
///
/// Mirrors [`crate::watcher`]'s own `LIVE`, including why `Mutex<Option<..>>`
/// and not `OnceLock`: a stopped scheduler must be able to clear itself.
static LIVE: Mutex<Option<Liveness>> = Mutex::new(None);

/// The scheduler's current status: whether its thread is actually running,
/// and at what interval it polls.
///
/// Always answers, unlike [`crate::watcher::live_status`] — the scheduler
/// has no "configured or not" distinction to fall back through, only
/// "running" or not.
pub fn live_status() -> SchedulerStatus {
    let running = LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(Liveness::is_alive);
    SchedulerStatus {
        running,
        poll_interval_seconds: configured_poll_interval().as_secs(),
    }
}

/// A running scheduler. Dropping it does **not** stop the loop — call
/// [`SchedulerHandle::stop`], which joins, so an in-flight pass cannot still
/// be writing while the caller tears the database down underneath it.
pub struct SchedulerHandle {
    stop: Arc<Stop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SchedulerHandle {
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Cleared after the join, not before: until the thread has actually
        // finished, it is still running and the status surface should say
        // so. Matches `WatcherHandle::stop`'s own reasoning.
        *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Start the polling loop against a database at `db_path`.
///
/// The thread opens its own connection rather than sharing the caller's:
/// `rusqlite::Connection` is not `Sync`, and passing one across would trade a
/// compile error for a runtime serialisation problem.
///
/// Unconditional, unlike the folder watcher — reminders have no enable switch,
/// only an interval.
/// Where this connection's database lives, or `None` for an in-memory one.
///
/// Following the shape `pid`/`backup`/`status` already established — each
/// keeps its own copy of this one-line `PRAGMA` rather than sharing a helper.
fn database_path(conn: &Connection) -> Option<std::path::PathBuf> {
    let path: String = conn
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .ok()?;
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

/// Start the scheduler for the database `conn` is attached to.
///
/// Returns `None` for an in-memory database: the loop's thread opens its own
/// connection by path, and `:memory:` would give it a *different*, empty
/// database rather than this one. Silently polling an empty database forever
/// would look exactly like a vault with nothing due.
pub fn start_scheduler_for(conn: &Connection) -> Option<SchedulerHandle> {
    Some(start_scheduler(database_path(conn)?))
}

pub fn start_scheduler(db_path: std::path::PathBuf) -> SchedulerHandle {
    let stop = Arc::new(Stop::new());
    let loop_stop = Arc::clone(&stop);
    let interval = configured_poll_interval();
    let (liveness, liveness_guard) = Liveness::new();
    *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(liveness);

    let thread = std::thread::Builder::new()
        .name("reminder-scheduler".to_string())
        .spawn(move || {
            let _liveness_guard = liveness_guard;
            let conn = match Connection::open(&db_path) {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("reminder scheduler: cannot open {:?}: {}", db_path, e);
                    return;
                }
            };
            while !loop_stop.is_stopped() {
                // A failed pass is reported and the loop continues. A
                // transient database error must not silently end reminder
                // delivery for the rest of the process's life.
                match poll_once(&conn) {
                    Ok(0) => {}
                    Ok(n) => eprintln!("reminder scheduler: delivered {} reminder(s)", n),
                    Err(e) => eprintln!("reminder scheduler: poll failed: {}", e),
                }
                loop_stop.wait(interval);
            }
        })
        .expect("spawning the reminder scheduler thread");

    SchedulerHandle {
        stop,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_liveness_is_alive() {
        let (liveness, _guard) = Liveness::new();
        assert!(liveness.is_alive());
    }

    #[test]
    fn dropping_the_guard_marks_it_not_alive() {
        let (liveness, guard) = Liveness::new();
        drop(guard);
        assert!(!liveness.is_alive());
    }

    /// The whole reason this type exists rather than a plain `bool` a loop
    /// sets on its way out: a thread that panics never reaches its own
    /// "I'm done" line, so anything relying on one would stay `true`
    /// forever. `LivenessGuard`'s `Drop` runs during an unwind exactly the
    /// same as it does on an ordinary return -- proven here by forcing the
    /// unwind through `catch_unwind` rather than assuming it.
    #[test]
    fn dropping_the_guard_during_a_panic_still_marks_it_not_alive() {
        let (liveness, guard) = Liveness::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = guard;
            panic!("simulated loop-thread panic");
        }));
        assert!(result.is_err(), "the test's own panic should propagate");
        assert!(
            !liveness.is_alive(),
            "unwinding past the guard must still mark it not alive"
        );
    }

    #[test]
    fn cloned_liveness_handles_see_the_same_state() {
        let (liveness, guard) = Liveness::new();
        let clone = liveness.clone();
        assert!(clone.is_alive());
        drop(guard);
        assert!(!clone.is_alive(), "a clone must observe the same drop");
    }

    #[test]
    fn live_status_reports_not_running_with_no_scheduler_started() {
        // Nothing in this crate's `src/` calls `start_scheduler` except the
        // function itself, so `LIVE` is never populated by any other unit
        // test sharing this test binary's process.
        assert!(!live_status().running);
    }
}
