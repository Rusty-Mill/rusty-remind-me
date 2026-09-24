//! Coverage for the knowledge-graph sync slice (`#57`, second slice):
//! `entities`, `entity_relations`, and `memory_entities` mention links over
//! the same push/pull protocol the memories-only slice established.

#[path = "../src/test_env.rs"]
mod test_env;

mod support;

use remind_me_core::entity::{self, entity_id, entity_relation_id};
use remind_me_core::sync::{
    apply_incoming_record, pull_entities, pull_entity_relations, pull_links, push_outbox,
    upsert_entity_record, upsert_entity_relation_record, upsert_link_record,
    EntityRelationSyncRecord, EntitySyncRecord, LinkSyncRecord,
};
use remind_me_core::{Database, EntityInput};
use rusqlite::Connection;
use serde_json::json;
use std::net::TcpListener;
use std::sync::Mutex;
use support::MockNode;

const SECRET: &str = "hub-secret";

/// `sync_flags.sync_enabled` (`#76`) means a node's own writes only reach
/// its outbox once sync is actually configured — every test in this file that
/// writes locally and expects to find the row in `sync_outbox` (directly, or
/// indirectly via `push_outbox`) holds this for the duration of that write.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn enable_sync(node_id: &str) {
    crate::test_env::set_var(remind_me_core::sync::NODE_ID_ENV, node_id);
    crate::test_env::set_var(remind_me_core::sync::HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(remind_me_core::sync::SYNC_SECRET_ENV, SECRET);
}

fn disable_sync() {
    crate::test_env::remove_var(remind_me_core::sync::NODE_ID_ENV);
    crate::test_env::remove_var(remind_me_core::sync::HUB_URL_ENV);
    crate::test_env::remove_var(remind_me_core::sync::SYNC_SECRET_ENV);
}

fn entity_row(
    conn: &Connection,
    id: &str,
) -> Option<(String, Option<String>, Vec<String>, String)> {
    conn.query_row(
        "SELECT name, kind, aliases, updated_at FROM entities WHERE id = ?",
        [id],
        |row| {
            let aliases: String = row.get(2)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                serde_json::from_str(&aliases).unwrap(),
                row.get(3)?,
            ))
        },
    )
    .ok()
}

fn entity_record(
    name: &str,
    kind: Option<&str>,
    aliases: &[&str],
    updated_at: &str,
) -> EntitySyncRecord {
    serde_json::from_value(json!({
        "id": entity_id(name),
        "name": name,
        "kind": kind,
        "aliases": aliases,
        "created_at": updated_at,
        "updated_at": updated_at,
        "node_id": "remote-node",
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// Entity conflict resolution
// ---------------------------------------------------------------------------

#[test]
fn a_newer_incoming_entity_wins_and_aliases_still_union_merge() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Bailey".to_string(),
            kind: None,
            aliases: vec!["B".to_string()],
        },
    )
    .unwrap();

    let incoming = entity_record(
        "bailey",
        Some("person"),
        &["Bails"],
        "2030-01-01T00:00:00+00:00",
    );
    upsert_entity_record(&conn, &incoming).unwrap();

    let (name, kind, aliases, _) = entity_row(&conn, &entity_id("Bailey")).unwrap();
    assert_eq!(name, "bailey", "the newer incoming name wins LWW");
    assert_eq!(kind.as_deref(), Some("person"));
    assert_eq!(
        aliases,
        vec!["B", "Bails"],
        "local alias first, then the new incoming one"
    );
}

#[test]
fn an_older_incoming_entity_loses_the_rename_but_still_merges_its_alias() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Bailey".to_string(),
            kind: Some("person".to_string()),
            aliases: vec!["B".to_string()],
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE entities SET updated_at = '2030-03-01T00:00:00+00:00' WHERE id = ?",
        [&entity_id("Bailey")],
    )
    .unwrap();
    let (_, _, _, updated_before) = entity_row(&conn, &entity_id("Bailey")).unwrap();

    let incoming = entity_record(
        "STALE NAME",
        None,
        &["Bails", "B"],
        "2020-01-01T00:00:00+00:00",
    );
    let mut incoming = incoming;
    incoming.id = entity_id("Bailey"); // same identity, stale timestamp
    upsert_entity_record(&conn, &incoming).unwrap();

    let (name, kind, aliases, updated_after) = entity_row(&conn, &entity_id("Bailey")).unwrap();
    assert_eq!(
        name, "Bailey",
        "the rename is rejected -- incoming lost LWW"
    );
    assert_eq!(
        kind.as_deref(),
        Some("person"),
        "kind is untouched on a loss"
    );
    assert_eq!(
        aliases,
        vec!["B", "Bails"],
        "the alias still merges in even though the record lost"
    );
    assert_eq!(
        updated_after, updated_before,
        "a losing record must not bump updated_at"
    );
}

#[test]
fn a_brand_new_entity_id_is_inserted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let incoming = entity_record(
        "Nova Scotia",
        Some("place"),
        &[],
        "2026-01-01T00:00:00+00:00",
    );

    upsert_entity_record(&conn, &incoming).unwrap();

    let (name, kind, ..) = entity_row(&conn, &entity_id("Nova Scotia")).unwrap();
    assert_eq!(name, "Nova Scotia");
    assert_eq!(kind.as_deref(), Some("place"));
}

#[test]
fn an_entity_record_missing_a_required_field_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut incoming = entity_record("X", None, &[], "2026-01-01T00:00:00+00:00");
    incoming.name = String::new();

    assert!(upsert_entity_record(&conn, &incoming).is_err());
}

#[test]
fn winning_an_existing_entity_does_not_touch_created_at() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Bailey".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
    let original_created_at: String = conn
        .query_row(
            "SELECT created_at FROM entities WHERE id = ?",
            [&entity_id("Bailey")],
            |r| r.get(0),
        )
        .unwrap();

    let mut incoming = entity_record("bailey", None, &[], "2030-01-01T00:00:00+00:00");
    incoming.created_at = "1999-01-01T00:00:00+00:00".to_string();
    upsert_entity_record(&conn, &incoming).unwrap();

    let created_at_after: String = conn
        .query_row(
            "SELECT created_at FROM entities WHERE id = ?",
            [&entity_id("Bailey")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at_after, original_created_at);
}

// ---------------------------------------------------------------------------
// entity_relations / links: insert-or-ignore, dangling tolerance
// ---------------------------------------------------------------------------

#[test]
fn entity_relation_insert_or_ignore_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let record: EntityRelationSyncRecord = serde_json::from_value(json!({
        "id": entity_relation_id("subj-1", "works_with", "obj-1"),
        "subject_entity_id": "subj-1",
        "relation": "works_with",
        "object_entity_id": "obj-1",
        "created_at": "2026-01-01T00:00:00+00:00",
        "updated_at": "2026-01-01T00:00:00+00:00",
    }))
    .unwrap();

    upsert_entity_relation_record(&conn, &record).unwrap();
    upsert_entity_relation_record(&conn, &record).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn entity_relation_referencing_unknown_entities_is_inserted_anyway() {
    // No foreign key: a relation may arrive before either entity it names.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let record: EntityRelationSyncRecord = serde_json::from_value(json!({
        "id": entity_relation_id("ghost-subject", "knows", "ghost-object"),
        "subject_entity_id": "ghost-subject",
        "relation": "knows",
        "object_entity_id": "ghost-object",
        "created_at": "2026-01-01T00:00:00+00:00",
        "updated_at": "2026-01-01T00:00:00+00:00",
    }))
    .unwrap();

    assert!(upsert_entity_relation_record(&conn, &record).is_ok());
    let count: i64 = conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "the dangling relation is stored, not rejected");
}

#[test]
fn link_insert_or_ignore_is_idempotent_and_returns_the_composite_wire_id() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let record = LinkSyncRecord {
        memory_id: "mem-1".to_string(),
        entity_id: "ent-1".to_string(),
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
    };

    let wire_id_1 = upsert_link_record(&conn, &record).unwrap();
    let wire_id_2 = upsert_link_record(&conn, &record).unwrap();

    assert_eq!(wire_id_1, "mem-1|ent-1");
    assert_eq!(wire_id_2, "mem-1|ent-1");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM memory_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn a_link_referencing_an_unknown_memory_or_entity_is_inserted_anyway() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let record = LinkSyncRecord {
        memory_id: "mem-ghost".to_string(),
        entity_id: "ent-ghost".to_string(),
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
    };

    assert!(upsert_link_record(&conn, &record).is_ok());
    let count: i64 = conn
        .query_row("SELECT count(*) FROM memory_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "no FK -- the dangling link waits, it does not error"
    );
}

// ---------------------------------------------------------------------------
// Push-receiving dispatch
// ---------------------------------------------------------------------------

#[test]
fn apply_incoming_record_dispatches_a_memory_record_when_record_type_is_absent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let raw = json!({
        "id": "mem_absent_type",
        "content": "no record_type key at all",
        "created_at": "2026-01-01T00:00:00+00:00",
        "updated_at": "2026-01-01T00:00:00+00:00",
    });

    let wire_id = apply_incoming_record(&conn, &raw).unwrap();

    assert_eq!(wire_id, "mem_absent_type");
    let content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = 'mem_absent_type'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content, "no record_type key at all");
}

#[test]
fn apply_incoming_record_dispatches_each_graph_record_type() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let entity_raw = json!({
        "record_type": "entity", "id": entity_id("Dispatch Test"), "name": "Dispatch Test",
        "created_at": "2026-01-01T00:00:00+00:00", "updated_at": "2026-01-01T00:00:00+00:00",
    });
    apply_incoming_record(&conn, &entity_raw).unwrap();
    assert!(entity_row(&conn, &entity_id("Dispatch Test")).is_some());

    let relation_raw = json!({
        "record_type": "entity_relation", "id": entity_relation_id("s", "r", "o"),
        "subject_entity_id": "s", "relation": "r", "object_entity_id": "o",
        "created_at": "2026-01-01T00:00:00+00:00", "updated_at": "2026-01-01T00:00:00+00:00",
    });
    apply_incoming_record(&conn, &relation_raw).unwrap();
    let relation_count: i64 = conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(relation_count, 1);

    let link_raw = json!({ "record_type": "memory_entity", "memory_id": "m1", "entity_id": "e1", "created_at": "2026-01-01T00:00:00+00:00" });
    let wire_id = apply_incoming_record(&conn, &link_raw).unwrap();
    assert_eq!(wire_id, "m1|e1");
}

#[test]
fn apply_incoming_record_refuses_an_unknown_record_type() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let raw = json!({ "record_type": "something_new", "id": "x" });

    assert!(apply_incoming_record(&conn, &raw).is_err());
}

// ---------------------------------------------------------------------------
// Outbox triggers actually fire for the graph tables
// ---------------------------------------------------------------------------

fn outbox_record_types(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT payload FROM sync_outbox ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .map(|payload| {
            let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
            parsed
                .get("record_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("memory")
                .to_string()
        })
        .collect()
}

#[test]
fn creating_an_entity_queues_a_tagged_outbox_row() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Quokka".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();

    assert_eq!(outbox_record_types(&conn), vec!["entity"]);
    disable_sync();
}

#[test]
fn creating_a_relation_and_a_link_each_queue_their_own_tagged_outbox_row() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "A".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "B".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
    entity::upsert_entity_relation(&conn, &entity_id("A"), "knows", &entity_id("B")).unwrap();
    entity::link_memory_entity(&conn, "mem_x", &entity_id("A")).unwrap();

    let types = outbox_record_types(&conn);
    assert!(types.contains(&"entity_relation".to_string()));
    assert!(types.contains(&"memory_entity".to_string()));
    disable_sync();
}

// ---------------------------------------------------------------------------
// Real push/pull round trip against a real peer server, via the shared
// `support::MockNode` (`sync_test.rs` has the same pattern; no reason for a
// second copy of it here).
// ---------------------------------------------------------------------------

#[test]
fn push_outbox_delivers_entities_relations_and_links_to_a_real_hub_in_one_pass() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("local-node");
    let hub = MockNode::start("hub-node", SECRET);
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();

    let a = entity::upsert_entity(
        &local_conn,
        &EntityInput {
            name: "Alice".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
    let b = entity::upsert_entity(
        &local_conn,
        &EntityInput {
            name: "Bob".to_string(),
            kind: None,
            aliases: vec![],
        },
    )
    .unwrap();
    entity::upsert_entity_relation(&local_conn, &a.id, "knows", &b.id).unwrap();
    entity::link_memory_entity(&local_conn, "mem_1", &a.id).unwrap();

    let report = push_outbox(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();
    assert_eq!(
        report.pushed, 4,
        "one entity insert x2, one relation, one link -- pushed together"
    );

    let hub_conn = hub.db.conn();
    let hub_entities: i64 = hub_conn
        .query_row("SELECT count(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    let hub_relations: i64 = hub_conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    let hub_links: i64 = hub_conn
        .query_row("SELECT count(*) FROM memory_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hub_entities, 2);
    assert_eq!(hub_relations, 1);
    assert_eq!(hub_links, 1);
    disable_sync();
}

#[test]
fn pull_entities_applies_the_hubs_entities_and_persists_a_namespaced_cursor() {
    // Holds `ENV_LOCK` for its whole body: `entity::upsert_entity` stamps
    // `node_id` on the hub-side row from the process-wide `NODE_ID_ENV` (via
    // `configured_node_id()`), and this test pulls with `node_id: "local-node"`
    // itself -- if a concurrently-running test's `enable_sync("local-node")`
    // window overlapped the insert below, the row would come back stamped
    // `node_id = "local-node"` and the pull's own `exclude_node=local-node`
    // filter would silently exclude it, producing a flaky `applied: 0`.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    disable_sync();
    let hub = MockNode::start("hub-node", SECRET);
    {
        let hub_conn = hub.db.conn();
        entity::upsert_entity(
            &hub_conn,
            &EntityInput {
                name: "Hub Entity".to_string(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
    }
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();

    let report = pull_entities(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();

    assert_eq!(report.applied, 1);
    assert!(entity_row(&local_conn, &entity_id("Hub Entity")).is_some());
    let cursor_rows: i64 = local_conn
        .query_row(
            "SELECT count(*) FROM sync_log WHERE remote_id = 'hub#entities'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor_rows, 1,
        "the entities cursor is namespaced separately from the bare memories cursor"
    );
}

#[test]
fn pull_links_and_pull_entity_relations_apply_the_hubs_graph_rows() {
    // Same `ENV_LOCK` reasoning as the entities-pull test above: the hub-side
    // writes below stamp `node_id` from the process-wide `NODE_ID_ENV`.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    disable_sync();
    let hub = MockNode::start("hub-node", SECRET);
    {
        let hub_conn = hub.db.conn();
        entity::upsert_entity_relation(&hub_conn, "s1", "works_with", "o1").unwrap();
        entity::link_memory_entity(&hub_conn, "mem_hub", "ent_hub").unwrap();
    }
    let local_db = Database::open_in_memory().unwrap();
    let local_conn = local_db.conn();

    let relations_report =
        pull_entity_relations(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();
    let links_report = pull_links(&local_conn, &hub.url, SECRET, "local-node", "hub").unwrap();

    assert_eq!(relations_report.applied, 1);
    assert_eq!(links_report.applied, 1);
    let relations: i64 = local_conn
        .query_row("SELECT count(*) FROM entity_relations", [], |r| r.get(0))
        .unwrap();
    let links: i64 = local_conn
        .query_row("SELECT count(*) FROM memory_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(relations, 1);
    assert_eq!(links, 1);
}

#[test]
fn a_two_node_entity_round_trip_converges_on_the_union_of_aliases() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync("node-a");
    let hub = MockNode::start("hub-node", SECRET);
    let node_a = Database::open_in_memory().unwrap();
    let node_a_conn = node_a.conn();
    entity::upsert_entity(
        &node_a_conn,
        &EntityInput {
            name: "Convergence".to_string(),
            kind: None,
            aliases: vec!["A-alias".to_string()],
        },
    )
    .unwrap();
    push_outbox(&node_a_conn, &hub.url, SECRET, "node-a", "hub").unwrap();

    let node_b = Database::open_in_memory().unwrap();
    let node_b_conn = node_b.conn();
    entity::upsert_entity(
        &node_b_conn,
        &EntityInput {
            name: "Convergence".to_string(),
            kind: Some("kind-from-b".to_string()),
            aliases: vec!["B-alias".to_string()],
        },
    )
    .unwrap();

    pull_entities(&node_b_conn, &hub.url, SECRET, "node-b", "hub").unwrap();

    let (_, kind, aliases, _) = entity_row(&node_b_conn, &entity_id("Convergence")).unwrap();
    assert!(
        kind.is_some(),
        "B's own kind survives regardless of which side won LWW"
    );
    assert!(
        aliases.contains(&"A-alias".to_string()) && aliases.contains(&"B-alias".to_string()),
        "got {aliases:?}"
    );
    disable_sync();
}

#[test]
fn pull_entities_tolerates_a_404_from_a_peer_that_predates_graph_sync() {
    // A bare listener that answers every request with 404 stands in for a
    // pre-graph-sync peer (one that only ever implemented /sync/pull).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = b"{\"error\":\"not found\"}";
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
    });

    let db = Database::open_in_memory().unwrap();
    let report = pull_entities(
        &db.conn(),
        &format!("http://127.0.0.1:{port}"),
        SECRET,
        "local-node",
        "old-peer",
    )
    .unwrap();

    assert_eq!(report.applied, 0);
    assert_eq!(report.pages, 0);
    let cursor_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sync_log WHERE remote_id = 'old-peer#entities'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor_rows, 0,
        "no cursor is written for a 404 -- there was nothing real to record"
    );
    handle.join().unwrap();
}
