//! The store daemon's periodic compaction of the engine tables.
//!
//! Every write to an engine table is appended to the table's insert log,
//! and a delete leaves a retired slot, until the table is compacted. Opening
//! a table folds its log, so a node that restarts often stays compact; a
//! daemon that runs for weeks does not. This runs
//! [`Database::compact_store`] on a timer, after pruning the sync outbox, as the hub runs its own
//! compaction every `REMIND_ME_HUB_COMPACT_INTERVAL_SECS`.

use crate::db::Database;
use crate::scheduler::Stop;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Seconds between compaction passes; `0` turns them off.
pub const COMPACT_INTERVAL_ENV: &str = "REMIND_ME_COMPACT_INTERVAL_SECS";
pub const DEFAULT_COMPACT_INTERVAL_SECS: u64 = 3600;

/// The configured interval, or `None` when compaction is off. An unparseable
/// value falls back to the default rather than turning compaction off.
pub fn compact_interval() -> Option<Duration> {
    let secs = std::env::var(COMPACT_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_COMPACT_INTERVAL_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// The running compaction thread.
pub struct CompactorHandle {
    stop: Arc<Stop>,
    thread: Option<JoinHandle<()>>,
}

impl CompactorHandle {
    /// Stop the thread, waiting for a pass in progress to finish.
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One maintenance pass: prune the outbox, then compact. Each step's failure
/// is reported and the next still runs: the next pass may succeed, and
/// compaction leaves every table readable whatever step it failed at.
///
/// The prune is here because nothing else runs it periodically since the
/// sync worker went (ADR-0032). A store that synced keeps queuing outbox
/// rows, and a daemon that runs for weeks would otherwise hold them past the
/// retention window until it restarts. Pruning first lets the same pass
/// reclaim the space.
pub fn run_pass(db: &Database) {
    match crate::sync::prune_outbox(&db.store()) {
        Ok(0) => {}
        Ok(n) => eprintln!("rusty-remind-me: pruned {n} outbox rows"),
        Err(e) => eprintln!("rusty-remind-me: outbox prune failed: {e}"),
    }
    match db.compact_store() {
        Ok(0) => {}
        Ok(n) => eprintln!("rusty-remind-me: compacted {n} store tables"),
        Err(e) => eprintln!("rusty-remind-me: compaction failed: {e}"),
    }
}

/// Start compacting `db` every [`compact_interval`], the first pass one
/// interval after the start. `None` when compaction is off.
///
/// # Errors
///
/// A message when the thread cannot be spawned.
pub fn start_compactor(db: Arc<Database>) -> Result<Option<CompactorHandle>, String> {
    let Some(interval) = compact_interval() else {
        return Ok(None);
    };
    let stop = Arc::new(Stop::new());
    let loop_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("store-compact".to_string())
        .spawn(move || loop {
            loop_stop.wait(interval);
            if loop_stop.is_stopped() {
                return;
            }
            run_pass(&db);
        })
        .map_err(|e| format!("could not start the compaction thread: {e}"))?;
    Ok(Some(CompactorHandle {
        stop,
        thread: Some(thread),
    }))
}
