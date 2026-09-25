//! Coverage for `GET /api/entity`, `GET /api/entities`, `GET
//! /api/entity/traverse`.
//!
//! These routes are read-only, and this crate's HTTP surface has no route
//! that creates an entity or a relation — matching the reference, which only
//! exposes lookup/browse/traverse over HTTP too. Fixtures are seeded through
//! `remind_me_core::entity` directly against the server's own database via
//! `common::seeded_server`.

mod common;
use common::{get, seeded_server, server};
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::{db::queries, EntityInput, MemoryAddInput};

fn add(conn: &rusqlite::Connection, content: &str) -> String {
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

fn entity(conn: &rusqlite::Connection, name: &str) -> String {
    upsert_entity(
        conn,
        &EntityInput {
            name: name.to_string(),
            kind: Some("place".into()),
            aliases: vec![],
        },
    )
    .unwrap()
    .id
}

// ---------------------------------------------------------------------------
// GET /api/entity
// ---------------------------------------------------------------------------

#[test]
fn missing_name_is_a_400() {
    let (server, root) = server("entity-no-name");
    let response = get(&server, "/api/entity");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_entity_is_404() {
    let (server, root) = server("entity-unknown");
    let response = get(&server, "/api/entity?name=Nowhere");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_known_entity_returns_its_profile() {
    let (server, root) = seeded_server("entity-known", |conn| {
        let id = entity(conn, "Rottnest Island");
        let mem_id = add(conn, "quokka sighting");
        link_memory_entity(conn, &mem_id, &id).unwrap();
    });

    let response = get(&server, "/api/entity?name=Rottnest%20Island");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["entity"]["name"], "Rottnest Island");
    assert_eq!(body["total_linked_memories"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/entities
// ---------------------------------------------------------------------------

#[test]
fn an_empty_store_lists_no_entities() {
    let (server, root) = server("entities-empty");
    let response = get(&server, "/api/entities");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["total"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn entities_are_listed_most_mentioned_first() {
    let (server, root) = seeded_server("entities-ordered", |conn| {
        let popular = entity(conn, "Rottnest Island");
        entity(conn, "Albany");
        for i in 0..2 {
            let mem_id = add(conn, &format!("memory {}", i));
            link_memory_entity(conn, &mem_id, &popular).unwrap();
        }
    });

    let response = get(&server, "/api/entities");
    let body = response.json();
    assert_eq!(body["total"], 2);
    assert_eq!(body["entities"][0]["name"], "Rottnest Island");
    assert_eq!(body["entities"][0]["mention_count"], 2);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/entity/traverse
// ---------------------------------------------------------------------------

#[test]
fn traverse_missing_name_is_a_400() {
    let (server, root) = server("traverse-no-name");
    let response = get(&server, "/api/entity/traverse");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn traverse_of_an_unknown_entity_is_404() {
    let (server, root) = server("traverse-unknown");
    let response = get(&server, "/api/entity/traverse?name=Nowhere");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn traverse_of_a_known_entity_with_no_edges_reports_itself() {
    let (server, root) = seeded_server("traverse-lonely", |conn| {
        entity(conn, "Rottnest Island");
    });

    let response = get(&server, "/api/entity/traverse?name=Rottnest%20Island");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["entity"]["name"], "Rottnest Island");
    // `edges` is omitted entirely when empty (`skip_serializing_if`), not an
    // empty array.
    assert!(body.get("edges").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn traverse_follows_a_one_hop_relation() {
    let (server, root) = seeded_server("traverse-edge", |conn| {
        let a = entity(conn, "Rottnest Island");
        let b = entity(conn, "Western Australia");
        conn.execute(
            "INSERT INTO entity_relations (id, subject_entity_id, relation, object_entity_id, created_at, updated_at)
             VALUES ('rel1', ?, 'located_in', ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            rusqlite::params![a, b],
        )
        .unwrap();
    });

    let response = get(
        &server,
        "/api/entity/traverse?name=Rottnest%20Island&hops=1",
    );
    let body = response.json();
    assert_eq!(body["edges"].as_array().unwrap().len(), 1);
    assert_eq!(body["edges"][0]["relation"], "located_in");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn traverse_hops_and_cap_are_clamped_not_rejected() {
    let (server, root) = seeded_server("traverse-clamp", |conn| {
        entity(conn, "Rottnest Island");
    });

    let response = get(
        &server,
        "/api/entity/traverse?name=Rottnest%20Island&hops=99&cap=99999",
    );
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
