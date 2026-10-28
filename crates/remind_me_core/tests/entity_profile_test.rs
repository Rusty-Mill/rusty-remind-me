//! Coverage for `entity::entity_profile` and `entity::list_entities`, the
//! shared implementations behind `GET /api/entity` and `GET /api/entities`.

use remind_me_core::db::derived::Origin;
use remind_me_core::db::entities::Entities;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::entity::{entity_profile, link_memory_entity, list_entities, upsert_entity};
use remind_me_core::testing;
use remind_me_core::{Database, EntityInput, MemoryAddInput};

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
// entity_profile
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_entity_returns_none() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    assert!(entity_profile(&store, "nowhere", 20).unwrap().is_none());
}

#[test]
fn a_known_entity_returns_its_row() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    upsert_entity(
        &store,
        &EntityInput {
            name: "Rottnest Island".into(),
            kind: Some("place".into()),
            aliases: vec!["Rotto".into()],
        },
    )
    .unwrap();

    let profile = entity_profile(&store, "rotto", 20).unwrap().unwrap();

    assert_eq!(profile.entity.name, "Rottnest Island");
    assert_eq!(profile.entity.aliases, vec!["Rotto"]);
    assert!(profile.facts.is_empty());
    assert!(profile.memories.is_empty());
    assert_eq!(profile.total_linked_memories, 0);
}

#[test]
fn facts_are_memories_whose_spo_matches_the_canonical_name() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "Rottnest Island");
    queries::add_memory(
        &store,
        MemoryAddInput {
            sensitive: false,
            content: "Rottnest Island has quokkas".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: Some("Rottnest Island".into()),
            predicate: Some("has".into()),
            object: Some("quokkas".into()),
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    // A subject match is case-insensitive: SPO fields are written verbatim.
    queries::add_memory(
        &store,
        MemoryAddInput {
            sensitive: false,
            content: "you can visit rottnest island by ferry".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: Some("rottnest island".into()),
            predicate: Some("reachable_by".into()),
            object: Some("ferry".into()),
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    add(&store, "unrelated memory");

    let profile = entity_profile(&store, "Rottnest Island", 20)
        .unwrap()
        .unwrap();

    assert_eq!(profile.facts.len(), 2);
}

#[test]
fn linked_memories_come_from_memory_entities_and_dangling_links_are_invisible() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = entity(&store, "Rottnest Island");
    let mem_id = add(&store, "quokkas everywhere");
    link_memory_entity(&store, &mem_id, &entity_id).unwrap();
    // A dangling link: an entity id with no matching row, as sync might
    // deliver before its endpoint arrives.
    Entities::new(&store)
        .link(
            "mem_ghost",
            &entity_id,
            "2026-01-01T00:00:00Z",
            Origin::Sync,
        )
        .unwrap();

    let profile = entity_profile(&store, "Rottnest Island", 20)
        .unwrap()
        .unwrap();

    assert_eq!(profile.memories.len(), 1);
    assert_eq!(profile.memories[0].id, mem_id);
    assert_eq!(profile.total_linked_memories, 1);
}

#[test]
fn superseded_and_deleted_memories_are_excluded_from_both_facts_and_links() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = entity(&store, "Rottnest Island");

    let fact_id = add(&store, "a fact");
    testing::set_memory_column(&store, &fact_id, "subject", "Rottnest Island").unwrap();
    testing::set_memory_column(&store, &fact_id, "superseded_by", "mem_newer").unwrap();

    let linked_id = add(&store, "a linked memory");
    link_memory_entity(&store, &linked_id, &entity_id).unwrap();
    testing::set_memory_column(&store, &linked_id, "deleted_at", "2026-01-02T00:00:00Z").unwrap();

    let profile = entity_profile(&store, "Rottnest Island", 20)
        .unwrap()
        .unwrap();

    assert!(profile.facts.is_empty());
    assert!(profile.memories.is_empty());
    assert_eq!(profile.total_linked_memories, 0);
}

#[test]
fn total_linked_memories_counts_beyond_the_page_limit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = entity(&store, "Rottnest Island");
    for i in 0..5 {
        let mem_id = add(&store, &format!("memory {}", i));
        link_memory_entity(&store, &mem_id, &entity_id).unwrap();
    }

    let profile = entity_profile(&store, "Rottnest Island", 2)
        .unwrap()
        .unwrap();

    assert_eq!(profile.memories.len(), 2, "the page is capped at limit");
    assert_eq!(
        profile.total_linked_memories, 5,
        "the total is not, so a caller can tell more exists"
    );
}

#[test]
fn content_snippet_is_trimmed_to_300_characters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = entity(&store, "Rottnest Island");
    let long_content = "a".repeat(500);
    let mem_id = add(&store, &long_content);
    link_memory_entity(&store, &mem_id, &entity_id).unwrap();

    let profile = entity_profile(&store, "Rottnest Island", 20)
        .unwrap()
        .unwrap();

    assert_eq!(profile.memories[0].content_snippet.len(), 300);
}

// ---------------------------------------------------------------------------
// list_entities
// ---------------------------------------------------------------------------

#[test]
fn an_empty_store_lists_no_entities() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let page = list_entities(&store, 50, 0).unwrap();

    assert_eq!(page.total, 0);
    assert!(page.entities.is_empty());
    assert!(!page.has_more);
}

#[test]
fn entities_are_ordered_by_mention_count_then_name() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let popular = entity(&store, "Rottnest Island");
    let quiet = entity(&store, "Bald Island");
    let unmentioned = entity(&store, "Albany");
    for i in 0..3 {
        let mem_id = add(&store, &format!("memory {}", i));
        link_memory_entity(&store, &mem_id, &popular).unwrap();
    }
    let mem_id = add(&store, "one mention");
    link_memory_entity(&store, &mem_id, &quiet).unwrap();
    let _ = unmentioned;

    let page = list_entities(&store, 50, 0).unwrap();

    let names: Vec<&str> = page.entities.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["Rottnest Island", "Bald Island", "Albany"]);
    assert_eq!(page.entities[0].mention_count, 3);
    assert_eq!(page.entities[2].mention_count, 0, "unmentioned still lists");
}

#[test]
fn list_entities_pages_with_the_standard_envelope() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..5 {
        entity(&store, &format!("Place {}", i));
    }

    let page = list_entities(&store, 2, 0).unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(page.count, 2);
    assert!(page.has_more);

    let last = list_entities(&store, 2, 4).unwrap();
    assert_eq!(last.count, 1);
    assert!(!last.has_more);
}
