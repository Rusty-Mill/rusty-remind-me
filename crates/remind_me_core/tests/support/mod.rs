// Deliberate module-level allow, not an oversight: this file is compiled
// fresh into every `tests/*.rs` binary that does `mod support;` (Rust warns
// per-binary), and no single one uses both `MockNode` and `MockHub`, or
// every field of either -- that "unused" is a property of which test file
// this is, not a real dead-code bug in the module. Per-item allows were
// considered and rejected: everything here is either used by every binary
// (`MockNode`) or genuinely only by one (`MockHub`, used solely by
// `hub_compat_test.rs`), so a blanket allow costs nothing today, and
// narrowing it item-by-item would only add churn every time a helper's
// consumer set shifts. Audited 2026-08-11 (issue #287): every item below is
// used by at least one binary in this crate's `tests/` directory --
// `MockNode` by `sync_test.rs`, `graph_sync_test.rs`, and
// `hub_compat_test.rs`; `MockHub` by `hub_compat_test.rs` alone. If a future
// audit finds an item unreferenced by every test binary, that's the real
// bug this allow can hide -- remove the item rather than assume it's fine.
#![allow(dead_code)]

//! Shared test doubles for sync integration tests, both real HTTP over a
//! real loopback `TcpListener` -- no mocking library, matching this
//! workspace's consistent choice everywhere else (`import_export_test.rs`'s
//! `authed_server`, `remind_me_remote/tests/http_test.rs`'s `spawn_server`,
//! ...).
//!
//! - [`MockNode`]: this crate's own `serve_once`/`PeerServerConfig`, the
//!   same code any node uses to answer another node's push/pull.
//!   `server.rs`'s own module doc: "There is deliberately no separate hub
//!   mode" -- a `MockNode` answers exactly as remind_me_core's sync client
//!   would see either a hub or a peer. Previously duplicated, byte-for-byte,
//!   as a private `TestHub` in three test files (`sync_test.rs`,
//!   `graph_sync_test.rs`, and inline in `peer_discovery_test.rs`); shared
//!   here instead.
//! - [`MockHub`]: a real `remind_me_hub` instance (the actual central-hub
//!   crate, SQLite-backed, no `postgres-store`). `MockNode` can never prove
//!   this crate's sync client is compatible with the real hub binary --
//!   only with another copy of itself. `remind_me_hub`'s own module doc
//!   states the two are meant to be indistinguishable protocol-wise ("a
//!   node cannot tell the two apart"); `MockHub` is what actually checks
//!   that claim from this side.
//!
//! Both share one socket-accept loop ([`AcceptLoop`]): bind, accept, hand
//! off to a per-connection handler, repeat until dropped. Not `pub`; an
//! implementation detail of the two mocks above, not something a test
//! should need directly.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use remind_me_core::sync::{serve_once, PeerServerConfig};
use remind_me_core::Database;
use remind_me_hub::http::{
    read_body, read_head, write_response, HeadOutcome, Response as HubResponse,
};
use remind_me_hub::store::sqlite::SqliteStore;
use remind_me_hub::store::HubStore;
use remind_me_hub::{dispatch, Config as HubConfig};

/// Binds nowhere itself -- takes an already-bound listener so a caller can
/// read its port (for building a `PeerServerConfig`/URL) before handing it
/// off. Non-blocking with a short poll sleep, exactly the pattern every
/// prior hand-rolled `TestHub` already used; [`Drop`] flips the shutdown
/// flag and joins the thread, so a mock never outlives the test that
/// created it.
struct AcceptLoop {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AcceptLoop {
    fn spawn(
        listener: TcpListener,
        mut handle_one: impl FnMut(TcpStream) + Send + 'static,
    ) -> Self {
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_one(stream),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for AcceptLoop {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A real peer-server node -- see the module doc. `db` is `pub` so a test
/// can inspect or seed it directly (as every prior `TestHub` usage already
/// did via `hub.db.conn()`), and `Arc`-wrapped so it outlives the
/// short-lived per-connection borrows the accept loop takes.
pub struct MockNode {
    pub url: String,
    pub db: Arc<Database>,
    _accept: AcceptLoop,
}

impl MockNode {
    /// Starts serving immediately, on an OS-assigned loopback port.
    pub fn start(node_id: &str, secret: &str) -> Self {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = PeerServerConfig::new("127.0.0.1", port, secret, node_id);
        let thread_db = Arc::clone(&db);
        let accept = AcceptLoop::spawn(listener, move |mut stream| {
            let conn = thread_db.conn();
            let _ = serve_once(&mut stream, &config, &conn);
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            db,
            _accept: accept,
        }
    }
}

/// A real `remind_me_hub` instance -- see the module doc.
///
/// `handle_one` below is a direct mirror of `remind_me_hub`'s own
/// `main.rs::handle` (`read_head` -> `read_body` -> `dispatch` ->
/// `write_response`), not a reimplementation: that function lives in the
/// *binary* crate, not the library, so it isn't reachable from here at all
/// -- this calls the exact same public API (`remind_me_hub::http::*`,
/// `remind_me_hub::dispatch`) `main.rs` does, in the same order.
pub struct MockHub {
    pub url: String,
    pub store: Arc<SqliteStore>,
    _accept: AcceptLoop,
}

impl MockHub {
    /// Starts serving immediately, on an OS-assigned loopback port, with
    /// metrics off and the default 90-day tombstone retention (neither is
    /// under test here).
    pub fn start(secret: &str) -> Self {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.migrate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(HubConfig {
            secret: secret.to_string(),
            metrics_enabled: false,
            tombstone_retention_days: 90,
        });
        let thread_store = Arc::clone(&store);
        let accept = AcceptLoop::spawn(listener, move |mut stream| {
            let _ = handle_hub_connection(&mut stream, thread_store.as_ref(), &config);
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            store,
            _accept: accept,
        }
    }
}

fn handle_hub_connection(
    stream: &mut TcpStream,
    store: &dyn HubStore,
    config: &HubConfig,
) -> std::io::Result<()> {
    let (head, buffered) = match read_head(stream)? {
        HeadOutcome::Parsed(head, buffered) => (head, buffered),
        HeadOutcome::Rejected(status, detail) => {
            return write_response(stream, &HubResponse::error(status, detail));
        }
    };
    let body = match read_body(stream, &head, buffered)? {
        Ok(body) => body,
        Err((status, detail)) => {
            return write_response(stream, &HubResponse::error(status, detail));
        }
    };
    let response = dispatch(store, config, &head, &body);
    write_response(stream, &response)
}
