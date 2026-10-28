//! Coverage for `queries::search_paginated`, the shared implementation
//! behind `GET /api/memories/search`.

use remind_me_core::db::queries::{self, search_paginated};
use remind_me_core::db::Store;
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::testing;
use remind_me_core::{Database, EntityInput, MemoryAddInput, SearchPageInput};

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

fn page(query: &str) -> SearchPageInput {
    SearchPageInput {
        query: query.to_string(),
        category: None,
        tags: None,
        entity: None,
        limit: 20,
        offset: 0,
    }
}

#[test]
fn a_plain_query_ranks_by_fts() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokkas live on Rottnest Island");
    add(&store, "unrelated content about ferries");

    let result = search_paginated(&store, &page("quokkas")).unwrap();

    assert_eq!(result.total, 1);
    assert_eq!(result.count, 1);
    assert_eq!(
        result.memories[0].content,
        "quokkas live on Rottnest Island"
    );
    assert!(result.message.is_none());
}

#[test]
fn pagination_envelope_matches_list_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..5 {
        add(&store, &format!("quokka sighting number {}", i));
    }

    let mut input = page("quokka");
    input.limit = 2;
    let first = search_paginated(&store, &input).unwrap();
    assert_eq!(first.total, 5);
    assert_eq!(first.count, 2);
    assert!(first.has_more);

    input.offset = 4;
    let last = search_paginated(&store, &input).unwrap();
    assert_eq!(last.count, 1);
    assert!(!last.has_more);
}

#[test]
fn category_and_tags_filter_before_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    queries::add_memory(
        &store,
        MemoryAddInput {
            sensitive: false,
            content: "quokka in general".into(),
            category: "general".into(),
            tags: vec!["island".into()],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    queries::add_memory(
        &store,
        MemoryAddInput {
            sensitive: false,
            content: "quokka in wildlife".into(),
            category: "wildlife".into(),
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
    .unwrap();

    let mut input = page("quokka");
    input.category = Some("wildlife".to_string());
    let by_category = search_paginated(&store, &input).unwrap();
    assert_eq!(by_category.total, 1);
    assert_eq!(by_category.memories[0].category, "wildlife");

    let mut input = page("quokka");
    input.tags = Some(vec!["island".to_string()]);
    let by_tag = search_paginated(&store, &input).unwrap();
    assert_eq!(by_tag.total, 1);
    assert_eq!(by_tag.memories[0].content, "quokka in general");
}

#[test]
fn superseded_and_deleted_memories_never_surface() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let stale = add(&store, "quokka census 2025");
    testing::set_memory_column(&store, &stale, "superseded_by", "mem_new").unwrap();
    let removed = add(&store, "quokka census deleted");
    testing::set_memory_column(&store, &removed, "deleted_at", "2026-01-01T00:00:00Z").unwrap();
    add(&store, "quokka census 2026");

    let result = search_paginated(&store, &page("quokka census")).unwrap();

    assert_eq!(result.total, 1);
    assert_eq!(result.memories[0].content, "quokka census 2026");
}

#[test]
fn an_entity_token_narrows_to_linked_or_spo_matching_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = upsert_entity(
        &store,
        &EntityInput {
            name: "Rottnest Island".into(),
            kind: Some("place".into()),
            aliases: vec![],
        },
    )
    .unwrap()
    .id;
    let linked = add(&store, "quokka sighting near the jetty");
    link_memory_entity(&store, &linked, &entity_id).unwrap();
    add(&store, "quokka sighting somewhere else entirely");

    let mut input = page("quokka sighting");
    input.entity = Some("Rottnest Island".to_string());
    let result = search_paginated(&store, &input).unwrap();

    assert_eq!(result.total, 1);
    assert_eq!(result.memories[0].id, linked);
}

#[test]
fn an_unknown_entity_is_a_real_empty_page_not_an_error() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting");

    let mut input = page("quokka");
    input.entity = Some("Nowhere Island".to_string());
    let result = search_paginated(&store, &input).unwrap();

    assert_eq!(result.total, 0);
    assert!(result.memories.is_empty());
    assert!(result
        .message
        .as_deref()
        .unwrap()
        .contains("Nowhere Island"));
}

#[test]
fn an_entity_only_query_lists_newest_first_instead_of_fts_ranking() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = upsert_entity(
        &store,
        &EntityInput {
            name: "Rottnest Island".into(),
            kind: Some("place".into()),
            aliases: vec![],
        },
    )
    .unwrap()
    .id;
    let first = add(&store, "first note");
    link_memory_entity(&store, &first, &entity_id).unwrap();
    let second = add(&store, "second note");
    link_memory_entity(&store, &second, &entity_id).unwrap();

    let mut input = page("entity:\"Rottnest Island\"");
    input.entity = Some("Rottnest Island".to_string());
    input.query = String::new(); // the caller has already stripped the token
    let result = search_paginated(&store, &input).unwrap();

    assert_eq!(result.total, 2);
    // Newest first: `second` was added after `first`.
    assert_eq!(result.memories[0].id, second);
}

#[test]
fn an_entity_scoped_search_still_excludes_superseded_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let entity_id = upsert_entity(
        &store,
        &EntityInput {
            name: "Rottnest Island".into(),
            kind: Some("place".into()),
            aliases: vec![],
        },
    )
    .unwrap()
    .id;
    let stale = add(&store, "old note");
    link_memory_entity(&store, &stale, &entity_id).unwrap();
    testing::set_memory_column(&store, &stale, "superseded_by", "mem_new").unwrap();

    let mut input = page("");
    input.entity = Some("Rottnest Island".to_string());
    let result = search_paginated(&store, &input).unwrap();

    assert_eq!(result.total, 0);
}
