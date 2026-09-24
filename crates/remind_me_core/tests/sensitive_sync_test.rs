//! The sensitive flag across sync (gap T11, issue #105).
//!
//! Its own test binary for the same reason as `undo_import_sync_test.rs`: the
//! sync switch is three process-wide env vars, and `sensitive_test.rs` asserts
//! single-node behaviour throughout.
//!
//! What "round-trips" has to mean here is both halves. #101 put `sensitive`
//! into the outbox trigger payload, so the sending side was already covered.
//! The receiving side was not: without `SyncRecord.sensitive`, a peer would
//! store the memory unmarked and it would surface in that node's ordinary
//! search — the flag defeated by the first sync rather than by any bug visible
//! locally.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::sync::{upsert_record, SyncRecord, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;

fn enable_sync() {
    crate::test_env::set_var(NODE_ID_ENV, "node-sensitive-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn add(conn: &Connection, content: &str, sensitive: bool) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive,
        },
    )
    .unwrap()
    .id
}

fn search_ids(conn: &Connection, query: &str, include_sensitive: bool) -> Vec<String> {
    queries::search_memories(
        conn,
        &MemorySearchInput {
            strategy: Default::default(),
            query: query.to_string(),
            category: None,
            tags: None,
            limit: 20,
            token_budget: 100_000,
            response_format: Default::default(),
            include_dormant: true,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            expand_co_retrieval: false,
            bootstrap: false,
            include_sensitive,
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| r.memory.id)
    .collect()
}

#[test]
fn the_outbox_payload_carries_the_flag() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute("DELETE FROM sync_outbox", []).unwrap();

    let id = add(&conn, "quokka sighting", true);

    let payload_flag: i64 = conn
        .query_row(
            "SELECT json_extract(payload, '$.sensitive') FROM sync_outbox
              WHERE memory_id = ?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        payload_flag, 1,
        "a peer rebuilds the memory from this payload alone"
    );
}

#[test]
fn an_incoming_sensitive_record_stays_hidden_on_this_node() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let record = SyncRecord {
        id: "mem_remote".into(),
        content: "quokka sighting from elsewhere".into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        created_at: "2030-01-01T00:00:00+00:00".into(),
        updated_at: "2030-01-01T00:00:00+00:00".into(),
        capture_id: None,
        node_id: Some("peer".into()),
        client: String::new(),
        accessed_at: None,
        access_count: 0,
        decay_rate: 0.1,
        vitality: 1.0,
        base_weight: 1.0,
        status: "active".into(),
        memory_type: "unclassified".into(),
        source_capture_id: None,
        subject: None,
        predicate: None,
        object: None,
        superseded_by: None,
        deleted_at: None,
        sensitive: true,
        remind_at: None,
    };

    upsert_record(&conn, &record).unwrap();

    let stored: i64 = conn
        .query_row(
            "SELECT sensitive FROM memories WHERE id = 'mem_remote'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "the flag must survive the crossing");
    assert!(
        search_ids(&conn, "quokka", false).is_empty(),
        "an incoming sensitive memory must not surface in ordinary search here"
    );
    assert_eq!(search_ids(&conn, "quokka", true), vec!["mem_remote"]);
}

#[test]
fn a_boolean_valued_sensitive_key_also_parses() {
    // A payload built by hand, or by a future writer that uses real JSON
    // booleans, has to keep working — the deserializer accepts both rather
    // than trading one wire shape for the other.
    let base = serde_json::json!({
        "id": "mem_bool",
        "content": "x",
        "category": "general",
        "tags": [],
        "source": "manual",
        "metadata": {},
        "created_at": "2030-01-01T00:00:00+00:00",
        "updated_at": "2030-01-01T00:00:00+00:00",
        "access_count": 0,
        "decay_rate": 0.1,
        "vitality": 1.0,
        "base_weight": 1.0,
        "status": "active",
        "memory_type": "unclassified",
        "client": ""
    });

    for (value, want) in [
        (serde_json::json!(true), true),
        (serde_json::json!(false), false),
        (serde_json::json!(1), true),
        (serde_json::json!(0), false),
        (serde_json::json!(null), false),
    ] {
        let mut json = base.clone();
        json["sensitive"] = value.clone();
        let record: SyncRecord = serde_json::from_value(json)
            .unwrap_or_else(|e| panic!("sensitive={} should parse: {}", value, e));
        assert_eq!(record.sensitive, want, "for sensitive={}", value);
    }
}

#[test]
fn a_record_from_a_pre_v27_peer_parses_as_not_sensitive() {
    // A node still on the old schema sends no `sensitive` key at all. That has
    // to deserialise as false rather than fail, or one stale peer breaks the
    // entire pull for everyone.
    let json = serde_json::json!({
        "id": "mem_old",
        "content": "from an older node",
        "category": "general",
        "tags": [],
        "source": "manual",
        "metadata": {},
        "created_at": "2030-01-01T00:00:00+00:00",
        "updated_at": "2030-01-01T00:00:00+00:00",
        "access_count": 0,
        "decay_rate": 0.1,
        "vitality": 1.0,
        "base_weight": 1.0,
        "status": "active",
        "memory_type": "unclassified",
        "client": ""
    });

    let record: SyncRecord = serde_json::from_value(json).unwrap();

    assert!(!record.sensitive);
}
