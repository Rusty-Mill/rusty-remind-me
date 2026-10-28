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
use remind_me_core::db::Store;
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::{db::queries, EntityInput, MemoryAddInput};

fn add(store: &Store<'_>, content: &str) -> String {
    queries::add_memory(
        store,
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
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn entity(store: &Store<'_>, name: &str) -> String {
    upsert_entity(
        store,
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
    let (server, root) = seeded_server("entity-known", |store| {
        let id = entity(store, "Rottnest Island");
        let mem_id = add(store, "quokka sighting");
        link_memory_entity(store, &mem_id, &id).unwrap();
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
    let (server, root) = seeded_server("entities-ordered", |store| {
        let popular = entity(store, "Rottnest Island");
        entity(store, "Albany");
        for i in 0..2 {
            let mem_id = add(store, &format!("memory {}", i));
            link_memory_entity(store, &mem_id, &popular).unwrap();
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
    let (server, root) = seeded_server("traverse-lonely", |store| {
        entity(store, "Rottnest Island");
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
    let (server, root) = seeded_server("traverse-edge", |store| {
        let a = entity(store, "Rottnest Island");
        let b = entity(store, "Western Australia");
        remind_me_core::db::entities::Entities::new(store)
            .insert_relation_or_ignore(
                &remind_me_core::db::entities::RelationRow {
                    id: "rel1",
                    subject_entity_id: &a,
                    relation: "located_in",
                    object_entity_id: &b,
                    created_at: "2026-01-01T00:00:00Z",
                    updated_at: "2026-01-01T00:00:00Z",
                    node_id: None,
                },
                remind_me_core::db::derived::Origin::Local,
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
    let (server, root) = seeded_server("traverse-clamp", |store| {
        entity(store, "Rottnest Island");
    });

    let response = get(
        &server,
        "/api/entity/traverse?name=Rottnest%20Island&hops=99&cap=99999",
    );
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
