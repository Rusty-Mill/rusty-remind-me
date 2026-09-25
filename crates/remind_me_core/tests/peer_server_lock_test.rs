//! Regression coverage: the peer server used to hold `Database::conn()`'s
//! process-wide mutex across the whole of `serve_once`, including reading
//! an incoming request -- so a slow or stuck peer connection blocked every
//! other database read or write for up to `IO_TIMEOUT`. `PeerServer` now
//! works its own `Database::open_secondary()` connection instead, mirroring
//! `SyncWorker`'s own fix for the identical shape of bug on the outbound
//! side (see `sync_worker_lock_test.rs`).
//!
//! `REMIND_ME_SYNC_SECRET`/`REMIND_ME_PEER_BIND`/`REMIND_ME_PEER_PORT` are
//! process-global, so this holds `ENV_LOCK` for its duration -- the same
//! convention `sync_test.rs` established.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{SyncPeer, PEER_BIND_ENV, PEER_PORT_ENV, SYNC_SECRET_ENV};
use remind_me_core::Database;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_stuck_peer_connection_never_blocks_an_ordinary_database_read() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir =
        std::env::temp_dir().join(format!("rrm_peer_server_lock_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Arc::new(Database::open(dir.join("memory.db")).unwrap());

    crate::test_env::set_var(SYNC_SECRET_ENV, "lock-test-secret");
    crate::test_env::set_var(PEER_BIND_ENV, "127.0.0.1");
    crate::test_env::set_var(PEER_PORT_ENV, "0");

    let peer = SyncPeer::from_env(Arc::clone(&db));
    let SyncPeer::Running(server) = &peer else {
        panic!("peer server failed to start: {:?}", peer.status());
    };
    let port = server.port();

    // Connects and then never sends a byte: "up" at the TCP layer, silent at
    // the HTTP layer -- the same shape a wedged or hostile peer produces.
    // Kept alive (not dropped) for the rest of the test so `serve_once`
    // stays blocked reading it.
    let _stuck = TcpStream::connect(("127.0.0.1", port)).unwrap();

    // Give the accept loop time to pick up the connection and block on it.
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    db.conn()
        .query_row("SELECT 1", [], |_| Ok(()))
        .expect("a plain local read");
    let elapsed = start.elapsed();

    crate::test_env::remove_var(SYNC_SECRET_ENV);
    crate::test_env::remove_var(PEER_BIND_ENV);
    crate::test_env::remove_var(PEER_PORT_ENV);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        elapsed < Duration::from_secs(2),
        "a plain read took {elapsed:?} while the peer server was blocked reading a stuck \
         connection -- Database::conn()'s mutex is being held across peer I/O again"
    );
}
