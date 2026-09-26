//! Coverage this crate's sync suite never had until now: pushing to and
//! pulling from a *real* `remind_me_hub` instance, not just another copy of
//! this crate's own peer server.
//!
//! `sync_test.rs`/`graph_sync_test.rs`'s `MockNode` (now `support::MockNode`)
//! proves the push/pull client works against `remind_me_core::sync::server`
//! -- but that IS this crate, running its own code on both ends. Both
//! crates' own module docs claim the wire protocol is interchangeable
//! ("a node cannot tell the two apart" -- `remind_me_hub/src/lib.rs`); this
//! file is what actually checks that claim, using `support::MockHub` (a
//! real engine-backed `remind_me_hub`, wired to a real
//! `TcpListener` through the exact same `read_head`/`read_body`/`dispatch`/
//! `write_response` sequence the real `rusty-remind-me-hub` binary uses).
//!
//! Every record here arrives at (or leaves) the hub over the wire via
//! `push_outbox`/`pull_remote` -- never by reaching into `MockHub::store`
//! directly, which would test this crate's understanding of remind_me_hub's
//! *internal* schema rather than the protocol the two crates actually agree
//! on.

#[path = "../src/test_env.rs"]
mod test_env;

mod support;

use remind_me_core::db::queries;
use remind_me_core::sync::{pull_remote, push_outbox, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::sync::Mutex;
use support::MockHub;

static ENV_LOCK: Mutex<()> = Mutex::new(());
const SECRET: &str = "real-hub-secret";

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

/// Same convention `sync_test.rs`/`graph_sync_test.rs` establish: the three
/// sync env vars are process-global, so every test that touches them holds
/// `ENV_LOCK` for its duration.
fn enable_sync(node_id: &str) {
    crate::test_env::set_var(NODE_ID_ENV, node_id);
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, SECRET);
}

fn disable_sync() {
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
}

#[test]
fn a_memory_pushed_to_a_real_hub_is_pulled_back_by_a_second_node() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = MockHub::start(SECRET);

    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    add(&node_a_conn, "pushed to the real hub");

    let pushed = push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();
    assert_eq!(pushed.pushed, 1, "the real hub must accept the push");

    let node_b = Database::open_in_memory().unwrap();
    let node_b_conn = node_b.conn();
    let pulled = pull_remote(&node_b_conn, &hub.url, SECRET, "node-b", "hub").unwrap();

    assert_eq!(pulled.applied, 1, "the real hub must serve it back");
    let count: i64 = node_b_conn
        .query_row(
            "SELECT count(*) FROM memories WHERE content = 'pushed to the real hub'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    disable_sync();
}

#[test]
fn a_sensitive_memory_stays_sensitive_after_a_real_hub_round_trip() {
    // #265: the hub's schema, wire columns and MemoryRecord all omitted
    // `sensitive` entirely, so this round trip silently unhid a memory the
    // author had marked sensitive -- exactly the failure `sensitive`'s own
    // doc comment (models.rs) exists to prevent. Push/pull only, matching
    // this file's own rule: never reach into `MockHub::store` directly.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = MockHub::start(SECRET);

    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    queries::add_memory(
        &node_a_conn,
        MemoryAddInput {
            sensitive: true,
            content: "sensitive across a real hub".to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();

    let node_b = Database::open_in_memory().unwrap();
    let node_b_conn = node_b.conn();
    let pulled = pull_remote(&node_b_conn, &hub.url, SECRET, "node-b", "hub").unwrap();
    assert_eq!(pulled.applied, 1);

    let sensitive: bool = node_b_conn
        .query_row(
            "SELECT sensitive FROM memories WHERE content = 'sensitive across a real hub'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sensitive,
        "a memory marked sensitive on node A must still be sensitive on node B \
         after a real push/pull round trip through the hub"
    );
    disable_sync();
}

#[test]
fn a_second_push_to_a_real_hub_sends_nothing_already_acknowledged() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = MockHub::start(SECRET);
    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    add(&node_a_conn, "content");

    push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();
    let second = push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();

    assert_eq!(
        second.pushed, 0,
        "the real hub's ack must persist across pushes, same as MockNode's"
    );
    disable_sync();
}

#[test]
fn a_real_hub_refuses_a_push_with_the_wrong_secret() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = MockHub::start(SECRET);
    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    add(&node_a_conn, "content");

    let err = push_outbox(&node_a_conn, &hub.url, "wrong-secret", "node-a", "hub").unwrap_err();
    assert!(err.to_string().contains("401"), "got {err}");
    disable_sync();
}

#[test]
fn a_pull_from_a_real_hub_with_nothing_new_applies_nothing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let hub = MockHub::start(SECRET);
    let node_b = Database::open_in_memory().unwrap();
    let node_b_conn = node_b.conn();

    let report = pull_remote(&node_b_conn, &hub.url, SECRET, "node-b", "hub").unwrap();

    assert_eq!(report.applied, 0);
}
