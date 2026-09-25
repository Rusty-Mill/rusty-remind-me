//! Regression coverage: the sync worker used to hold `Database::conn()`'s
//! process-wide mutex across its whole network cycle (push + pull against
//! the hub and every discovered peer), so a hub that accepted a connection
//! but never answered blocked every other database read or write for up to
//! that call's full I/O timeout. `SyncWorker` now works its own
//! `Database::open_secondary()` connection instead, so an ordinary read
//! made while a cycle is stuck talking to a dead remote stays fast.
//!
//! `REMIND_ME_NODE_ID`/`REMIND_ME_HUB_URL`/`REMIND_ME_SYNC_SECRET` are
//! process-global, so this holds `ENV_LOCK` for its duration -- the same
//! convention `sync_test.rs` established.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{SyncWorker, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::Database;
use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_stuck_hub_never_blocks_an_ordinary_database_read() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Accepts a connection and then never writes a byte back: "up" at the
    // TCP layer, silent at the HTTP layer -- the same shape a wedged peer
    // produces. The accept thread is never joined; it dies with the test
    // process, which is fine, since nothing after it depends on that.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hub_addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut sink = stream;
            let mut buf = [0u8; 1];
            let _ = sink.read(&mut buf); // hold the connection open, reply never
        }
    });

    let dir =
        std::env::temp_dir().join(format!("rrm_sync_worker_lock_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("memory.db");
    let db = Arc::new(Database::open(&db_path).unwrap());

    crate::test_env::set_var(NODE_ID_ENV, "lock-test-node");
    crate::test_env::set_var(HUB_URL_ENV, format!("http://{hub_addr}"));
    crate::test_env::set_var(SYNC_SECRET_ENV, "lock-test-secret");

    let mut worker = SyncWorker::from_env(db_path.clone()).expect("sync enabled by env");

    // Give the freshly spawned cycle time to reach the hub and block on it.
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    db.conn()
        .query_row("SELECT 1", [], |_| Ok(()))
        .expect("a plain local read");
    let elapsed = start.elapsed();

    worker.stop();
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        elapsed < Duration::from_secs(2),
        "a plain read took {elapsed:?} while the sync worker was talking to a stuck hub \
         -- Database::conn()'s mutex is being held across network I/O again"
    );
}
