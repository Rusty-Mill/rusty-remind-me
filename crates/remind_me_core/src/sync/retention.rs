//! The outbox's retention lifecycle in a long-lived process.
//!
//! A store that synced before multi-node sync was removed (ADR-0032) still
//! queues an outbox row for every write, and nothing drains it. The sync
//! worker used to prune it on each cycle; this thread does that now,
//! without any transport, in every process that holds a store open for
//! long: the store daemon and the in-process `server`, `api` and `remote`.
//! It is separate from compaction so that turning compaction off does not
//! turn retention off.

use crate::db::{SecondarySource, Store};
use crate::scheduler::Stop;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Time between prunes. Opening a store prunes too, so a process that
/// restarts more often than this never needs the thread at all.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// The running prune thread.
pub struct PrunerHandle {
    stop: Arc<Stop>,
    thread: Option<JoinHandle<()>>,
}

impl PrunerHandle {
    /// Stop the thread, waiting for a prune in progress to finish.
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PrunerHandle {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Prune `store`'s outbox every [`PRUNE_INTERVAL`] while the process runs.
/// `None` for an in-memory store, which nothing outlives, and for a store
/// that never synced, whose outbox stays empty.
pub fn start_pruner_for(store: &Store<'_>) -> Option<PrunerHandle> {
    if !super::store_syncs(store).unwrap_or(true) {
        return None;
    }
    start_pruner(store.secondary_source()?, PRUNE_INTERVAL)
}

/// Prune the outbox `source` reaches every `interval`, the first time one
/// interval after the start. `None` if the thread cannot be spawned, which
/// is reported: the store still prunes on every open.
pub fn start_pruner(source: SecondarySource, interval: Duration) -> Option<PrunerHandle> {
    let stop = Arc::new(Stop::new());
    let loop_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("outbox-prune".to_string())
        .spawn(move || {
            let store = source.store();
            loop {
                loop_stop.wait(interval);
                if loop_stop.is_stopped() {
                    return;
                }
                // A failed prune is reported and the loop carries on: the
                // next one may succeed, and nothing is lost by waiting.
                match super::prune_outbox(&store) {
                    Ok(0) => {}
                    Ok(n) => eprintln!("rusty-remind-me: pruned {n} outbox rows"),
                    Err(e) => eprintln!("rusty-remind-me: outbox prune failed: {e}"),
                }
            }
        });
    match spawned {
        Ok(thread) => Some(PrunerHandle {
            stop,
            thread: Some(thread),
        }),
        Err(e) => {
            eprintln!("rusty-remind-me: could not start the outbox prune thread: {e}");
            None
        }
    }
}
