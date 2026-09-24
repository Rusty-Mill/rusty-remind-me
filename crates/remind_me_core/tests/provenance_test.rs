//! Who wrote each memory (#258): the `client` and `node_id` columns.
//!
//! # What was actually wrong
//!
//! The issue said nothing recorded the writer. Not quite: `memories.client`
//! and `memories.node_id` have existed all along, and `add_memory` set both.
//! The other five paths that insert into `memories` directly — `auto_capture`
//! and its `decompose` half, `promote`, `write_skeleton`, and
//! `apply_normalizations` — set neither, so `client` fell back to the schema
//! default `'unknown'` and `node_id` to `NULL`.
//!
//! That is worse than an absent column. `'unknown'` could not be told apart
//! from "nobody configured a client", and `node_id` rides the sync outbox
//! payload, so per-node attribution on the hub silently saw only
//! manually-added memories.
//!
//! These tests therefore assert the columns per *write path*, because the bug
//! was omission at a call site rather than a wrong value in a shared helper —
//! and a test of the helper alone would have passed throughout.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{
    configured_client, memory_provenance, set_handshake_client, CLIENT_ENV, DEFAULT_CLIENT,
    NODE_ID_ENV,
};
use remind_me_core::{db::queries, Database};
use rusqlite::{params, Connection};
use std::sync::Mutex;

/// `REMIND_ME_CLIENT`, `REMIND_ME_NODE_ID` and the handshake slot are all
/// process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const TEST_NODE: &str = "test-node";
const TEST_CLIENT: &str = "test-client";

/// Sets both variables and clears any handshake identity, restoring all three
/// on the way out so a failure cannot leak into the rest of the binary.
struct Ident;
impl Ident {
    fn set() -> Self {
        crate::test_env::set_var(NODE_ID_ENV, TEST_NODE);
        crate::test_env::set_var(CLIENT_ENV, TEST_CLIENT);
        set_handshake_client(None);
        Ident
    }
}
impl Drop for Ident {
    fn drop(&mut self) {
        crate::test_env::remove_var(NODE_ID_ENV);
        crate::test_env::remove_var(CLIENT_ENV);
        set_handshake_client(None);
    }
}

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_prov_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

/// Every non-deleted memory's `(node_id, client)`, so a path that writes more
/// than one row (a capture writes two) cannot pass by having only one right.
fn stamps(conn: &Connection) -> Vec<(Option<String>, String)> {
    let mut stmt = conn
        .prepare("SELECT node_id, client FROM memories ORDER BY rowid")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn assert_all_stamped(conn: &Connection, path: &str, expected_rows: usize) {
    let rows = stamps(conn);
    assert_eq!(
        rows.len(),
        expected_rows,
        "{path}: expected {expected_rows} memories, found {}",
        rows.len()
    );
    for (node_id, client) in rows {
        assert_eq!(
            node_id.as_deref(),
            Some(TEST_NODE),
            "{path}: node_id must be stamped -- NULL rides the sync outbox payload \
             and makes per-node attribution on the hub silently incomplete"
        );
        assert_eq!(
            client, TEST_CLIENT,
            "{path}: client must be stamped -- the schema default 'unknown' is \
             indistinguishable from an unconfigured client"
        );
    }
}

#[test]
fn add_memory_stamps_both() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("add");
    let conn = db.conn();

    queries::add_memory(
        &conn,
        serde_json::from_value(serde_json::json!({ "content": "a quokka fact" })).unwrap(),
    )
    .unwrap();

    // The one path that was already correct; asserted so a refactor that
    // routes everything through the new helper cannot regress it unnoticed.
    assert_all_stamped(&conn, "add_memory", 1);
}

#[test]
fn auto_capture_stamps_both_halves() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("capture");
    let conn = db.conn();

    remind_me_core::capture::auto_capture(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "conversation": "user: hello\nassistant: hi",
            "summary": "a greeting",
        }))
        .unwrap(),
    )
    .unwrap();

    // Two rows: the verbatim dialog and the summary.
    assert_all_stamped(&conn, "auto_capture", 2);
}

#[test]
fn decomposed_facts_are_stamped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("decompose");
    let conn = db.conn();

    let capture = remind_me_core::capture::auto_capture(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "conversation": "user: I prefer Rust\nassistant: noted",
            "summary": "language preference",
        }))
        .unwrap(),
    )
    .unwrap();

    remind_me_core::capture::decompose(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "capture_id": capture.capture_id,
            "facts": [{ "content": "Prefers Rust." }],
        }))
        .unwrap(),
    )
    .unwrap();

    // Two capture halves plus the one fact.
    assert_all_stamped(&conn, "decompose", 3);
}

#[test]
fn a_promoted_memory_is_stamped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("promote");
    let conn = db.conn();

    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
            created_at, updated_at, vitality, node_id, client)
         VALUES ('mem_src', 'scenario source', 'scenario', '[]', 'manual', '{}', ?, ?, 1.0, ?, ?)",
        params![now, now, TEST_NODE, TEST_CLIENT],
    )
    .unwrap();

    remind_me_core::promotion::promote(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "rung": "scenario_to_persona",
            "source_ids": ["mem_src"],
            "content": "Ships small reversible changes.",
        }))
        .unwrap(),
    )
    .unwrap();

    assert_all_stamped(&conn, "promote", 2);
}

#[test]
fn a_skeleton_is_stamped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("skeleton");
    let conn = db.conn();

    let capture = remind_me_core::capture::auto_capture(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "conversation": "user: one\nassistant: two\nuser: three",
            "summary": "a short exchange",
        }))
        .unwrap(),
    )
    .unwrap();

    remind_me_core::skeleton::write_skeleton(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "capture_id": capture.capture_id,
            "mermaid": "graph TD\n  n1[opening]",
            "nodes": { "n1": [1, 2] },
        }))
        .unwrap(),
    )
    .unwrap();

    // Two capture halves plus the skeleton.
    assert_all_stamped(&conn, "skeleton", 3);
}

#[test]
fn the_handshake_identity_beats_the_configured_one() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();

    // Configured only: the env value stands. This is the CLI, the dashboard
    // and the importer, none of which handshake.
    assert_eq!(configured_client(), TEST_CLIENT);

    // A handshake wins, because it is observed rather than configured: one
    // server serving several clients has one env value and many real callers.
    set_handshake_client(Some("claude-code/2.1.0".to_string()));
    assert_eq!(configured_client(), "claude-code/2.1.0");
    assert_eq!(
        memory_provenance(),
        (TEST_NODE.to_string(), "claude-code/2.1.0".to_string())
    );

    // Clearing falls back rather than sticking or emptying.
    set_handshake_client(None);
    assert_eq!(configured_client(), TEST_CLIENT);

    // A blank name must not record a client called "".
    set_handshake_client(Some("   ".to_string()));
    assert_eq!(configured_client(), TEST_CLIENT);
}

#[test]
fn with_nothing_configured_the_client_is_unknown_and_the_node_is_empty() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(CLIENT_ENV);
    set_handshake_client(None);

    // The default is still `unknown`; this change makes the column *consistent*,
    // not populated out of nowhere. A single-node install with no sync
    // configured legitimately has no node id.
    let (node_id, client) = memory_provenance();
    assert_eq!(client, DEFAULT_CLIENT);
    assert_eq!(node_id, "");
}

#[test]
fn a_normalized_memory_is_stamped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ident = Ident::set();
    let db = db("normalize");
    let conn = db.conn();

    let raw = queries::add_memory(
        &conn,
        serde_json::from_value(serde_json::json!({
            "content": "a long raw import chunk about quokkas",
        }))
        .unwrap(),
    )
    .unwrap();

    let outcome = remind_me_core::normalize::apply_normalizations(
        &conn,
        &serde_json::from_value(serde_json::json!({
            "normalizations": [{
                "memory_id": raw.id,
                "question": "What is a quokka?",
                "summary": "A small marsupial.",
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        outcome.normalized, 1,
        "fixture must actually normalize, or this asserts nothing: {:?}",
        outcome.errors
    );

    // The raw memory plus its distillation.
    assert_all_stamped(&conn, "apply_normalizations", 2);
}
