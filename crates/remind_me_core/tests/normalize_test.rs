//! Coverage for `remind_me_normalize_batch` / `remind_me_normalize_apply`.
//!
//! Nothing writes `document_import` or `chat_import` memories yet — the
//! importers are Wave 5 — so raw imports are inserted directly here. The
//! empty-batch case below pins that a store built only through `remind_me_add`
//! correctly has nothing to normalize.

use remind_me_core::db::queries;
use remind_me_core::normalize::{
    apply_normalizations, unnormalized_batch, NORMALIZED_CATEGORY, NORMALIZED_SOURCE,
};
use remind_me_core::{
    Database, EntityInput, MemoryAddInput, NormalizationEntry, NormalizeApplyInput,
    NormalizeBatchInput, NORMALIZE_APPLY_MAX,
};
use rusqlite::Connection;

fn batch(conn: &Connection, size: usize) -> remind_me_core::NormalizeBatchResult {
    unnormalized_batch(conn, &NormalizeBatchInput { batch_size: size }).unwrap()
}

/// Insert a raw import the way the (not yet written) importers will.
fn import(conn: &Connection, id: &str, content: &str, source: &str, created_at: &str) {
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
                               created_at, updated_at, doc_id, chunk_index)
         VALUES (?, ?, 'general', '[\"raw\"]', ?, ?, ?, ?, 'doc_7', 3)",
        rusqlite::params![
            id,
            content,
            source,
            r#"{"filename": "notes.md"}"#,
            created_at,
            created_at
        ],
    )
    .unwrap();
}

fn entry(memory_id: &str) -> NormalizationEntry {
    NormalizationEntry {
        memory_id: memory_id.to_string(),
        question: "What did we decide?".into(),
        summary: "We went with SQLite.".into(),
        resolution: None,
        refs: vec![],
        entities: vec![],
    }
}

fn apply(
    conn: &Connection,
    entries: Vec<NormalizationEntry>,
) -> remind_me_core::NormalizeApplyResult {
    apply_normalizations(
        conn,
        &NormalizeApplyInput {
            normalizations: entries,
        },
    )
    .unwrap()
}

fn column(conn: &Connection, id: &str, name: &str) -> String {
    conn.query_row(
        &format!("SELECT {} FROM memories WHERE id = ?", name),
        rusqlite::params![id],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

#[test]
fn a_store_without_imports_has_nothing_to_normalize() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            sensitive: false,
            content: "written by hand".into(),
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

    let result = batch(&conn, 20);

    // Correct, not broken: only importer-sourced memories are eligible, and
    // this crate has no importers yet.
    assert!(result.memories.is_empty());
    assert_eq!(result.total_unnormalized, 0);
}

#[test]
fn both_import_sources_are_eligible() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_doc",
        "raw doc",
        "document_import",
        "2026-01-01T00:00:00Z",
    );
    import(
        &conn,
        "mem_chat",
        "raw chat",
        "chat_import",
        "2026-01-02T00:00:00Z",
    );
    import(
        &conn,
        "mem_hook",
        "raw hook",
        "webhook",
        "2026-01-03T00:00:00Z",
    );

    let result = batch(&conn, 20);

    let ids: Vec<String> = result.memories.iter().map(|m| m.id.clone()).collect();
    // Newest first.
    assert_eq!(ids, vec!["mem_chat", "mem_doc"]);
    assert_eq!(result.total_unnormalized, 2);
}

#[test]
fn the_batch_reports_the_full_backlog_and_caps_the_snippet() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        import(
            &conn,
            &format!("mem_{}", i),
            &"x".repeat(1500),
            "document_import",
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        );
    }

    let result = batch(&conn, 2);

    assert_eq!(result.memories.len(), 2);
    assert_eq!(
        result.total_unnormalized, 5,
        "callers need the backlog to know whether another round is worth it"
    );
    assert_eq!(result.memories[0].content_snippet.chars().count(), 1000);
    assert_eq!(result.memories[0].tags, vec!["raw".to_string()]);
    assert_eq!(result.memories[0].filename.as_deref(), Some("notes.md"));
    assert_eq!(result.memories[0].source, "document_import");
}

#[test]
fn the_batch_size_is_clamped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..3 {
        import(
            &conn,
            &format!("mem_{}", i),
            "raw",
            "chat_import",
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        );
    }

    assert_eq!(batch(&conn, 0).memories.len(), 1, "zero clamps up to 1");
    assert_eq!(batch(&conn, 5_000).memories.len(), 3);
}

#[test]
fn a_multibyte_snippet_boundary_does_not_panic() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Truncating by bytes would slice through a character here.
    import(
        &conn,
        "mem_1",
        &"é".repeat(1500),
        "document_import",
        "2026-01-01T00:00:00Z",
    );

    assert_eq!(
        batch(&conn, 20).memories[0].content_snippet.chars().count(),
        1000
    );
}

#[test]
fn applying_creates_a_new_memory_and_leaves_the_raw_one_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "verbatim junk",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let outcome = apply(&conn, vec![entry("mem_raw")]);

    assert_eq!(outcome.normalized, 1);
    assert!(outcome.errors.is_empty());
    let normalized_id = outcome.results[0].normalized_id.clone();
    assert_ne!(normalized_id, "mem_raw");

    assert_eq!(column(&conn, "mem_raw", "content"), "verbatim junk");
    assert_eq!(
        column(&conn, &normalized_id, "category"),
        NORMALIZED_CATEGORY
    );
    assert_eq!(column(&conn, &normalized_id, "source"), NORMALIZED_SOURCE);
}

#[test]
fn the_normalized_content_renders_the_distillation() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let mut with_resolution = entry("mem_raw");
    with_resolution.resolution = Some("Shipped in v2.".into());
    let id = apply(&conn, vec![with_resolution]).results[0]
        .normalized_id
        .clone();

    assert_eq!(
        column(&conn, &id, "content"),
        "**Q:** What did we decide?\n\nWe went with SQLite.\n\n**Resolution:** Shipped in v2."
    );
}

#[test]
fn a_missing_resolution_is_omitted_from_content_and_metadata() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let id = apply(&conn, vec![entry("mem_raw")]).results[0]
        .normalized_id
        .clone();

    assert_eq!(
        column(&conn, &id, "content"),
        "**Q:** What did we decide?\n\nWe went with SQLite."
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&column(&conn, &id, "metadata")).unwrap();
    assert!(metadata.get("resolution").is_none());
}

#[test]
fn the_link_back_lives_in_metadata() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "document_import",
        "2026-01-01T00:00:00Z",
    );

    let mut with_refs = entry("mem_raw");
    with_refs.refs = vec!["https://example.test/adr-1".into()];
    let id = apply(&conn, vec![with_refs]).results[0]
        .normalized_id
        .clone();

    let metadata: serde_json::Value =
        serde_json::from_str(&column(&conn, &id, "metadata")).unwrap();
    assert_eq!(metadata["normalized_from"], "mem_raw");
    assert_eq!(metadata["question"], "What did we decide?");
    assert_eq!(metadata["refs"][0], "https://example.test/adr-1");
}

#[test]
fn the_normalized_memory_inherits_tags_and_document_position() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "document_import",
        "2026-01-01T00:00:00Z",
    );

    let id = apply(&conn, vec![entry("mem_raw")]).results[0]
        .normalized_id
        .clone();

    // doc_id and chunk_index carry over so neighbour-aware retrieval still
    // associates the distillation with the rest of the document.
    assert_eq!(column(&conn, &id, "doc_id"), "doc_7");
    let chunk: i64 = conn
        .query_row(
            "SELECT chunk_index FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(chunk, 3);
    assert_eq!(column(&conn, &id, "tags"), r#"["raw"]"#);
}

#[test]
fn a_normalized_import_drops_out_of_the_next_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_a",
        "raw a",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );
    import(
        &conn,
        "mem_b",
        "raw b",
        "chat_import",
        "2026-01-02T00:00:00Z",
    );

    apply(&conn, vec![entry("mem_a")]);

    let result = batch(&conn, 20);
    let ids: Vec<String> = result.memories.iter().map(|m| m.id.clone()).collect();
    // There is no "normalized" flag column — the backlog shrinks purely because
    // something now points back at mem_a.
    assert_eq!(ids, vec!["mem_b"]);
    assert_eq!(result.total_unnormalized, 1);
}

#[test]
fn the_distillation_is_not_itself_offered_for_normalization() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    apply(&conn, vec![entry("mem_raw")]);

    // Its source is `normalization`, not an import source, so it is ineligible
    // — otherwise normalizing would generate its own backlog forever.
    assert_eq!(batch(&conn, 20).total_unnormalized, 0);
}

#[test]
fn superseded_and_deleted_imports_are_skipped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_live",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );
    import(
        &conn,
        "mem_old",
        "raw",
        "chat_import",
        "2026-01-02T00:00:00Z",
    );
    import(
        &conn,
        "mem_gone",
        "raw",
        "chat_import",
        "2026-01-03T00:00:00Z",
    );
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_live' WHERE id = 'mem_old'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-04T00:00:00Z' WHERE id = 'mem_gone'",
        [],
    )
    .unwrap();

    let result = batch(&conn, 20);

    let ids: Vec<String> = result.memories.iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, vec!["mem_live"]);
    assert_eq!(result.total_unnormalized, 1);
}

#[test]
fn entities_named_by_the_distillation_are_linked() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "document_import",
        "2026-01-01T00:00:00Z",
    );

    let mut with_entities = entry("mem_raw");
    with_entities.entities = vec![EntityInput {
        name: "SQLite".into(),
        kind: Some("technology".into()),
        aliases: vec![],
    }];
    let id = apply(&conn, vec![with_entities]).results[0]
        .normalized_id
        .clone();

    // The raw import is never entity-linked automatically, so without this the
    // distillation would be invisible to entity lookup and traversal.
    let linked: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_entities WHERE memory_id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked, 1);
    assert_eq!(
        remind_me_core::entity::resolve_entity(&conn, "sqlite")
            .unwrap()
            .unwrap()
            .name,
        "SQLite"
    );
}

#[test]
fn the_distillation_is_searchable() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "verbatim junk",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let mut about_sqlite = entry("mem_raw");
    about_sqlite.summary = "We went with quokka storage.".into();
    let id = apply(&conn, vec![about_sqlite]).results[0]
        .normalized_id
        .clone();

    let found = queries::search_memories(
        &conn,
        &remind_me_core::MemorySearchInput {
            strategy: Default::default(),
            include_sensitive: false,
            query: "quokka".into(),
            category: None,
            tags: None,
            limit: 20,
            token_budget: 100_000,
            response_format: Default::default(),
            include_dormant: false,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            expand_co_retrieval: false,
            bootstrap: false,
        },
    )
    .unwrap();

    // Making noisy imports individually searchable in a cleaner form is the
    // entire point, so the FTS triggers must have picked the new row up.
    assert_eq!(
        found
            .iter()
            .map(|r| r.memory.id.clone())
            .collect::<Vec<_>>(),
        vec![id]
    );
}

#[test]
fn an_unknown_id_is_reported_without_discarding_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_real",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let outcome = apply(&conn, vec![entry("mem_ghost"), entry("mem_real")]);

    assert_eq!(outcome.normalized, 1);
    assert_eq!(outcome.results[0].memory_id, "mem_real");
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].memory_id, "mem_ghost");
    assert_eq!(outcome.errors[0].error, "memory not found");
}

#[test]
fn a_full_apply_batch_is_accepted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let entries: Vec<NormalizationEntry> = (0..NORMALIZE_APPLY_MAX)
        .map(|i| {
            let id = format!("mem_{}", i);
            import(&conn, &id, "raw", "document_import", "2026-01-01T00:00:00Z");
            entry(&id)
        })
        .collect();

    let outcome = apply(&conn, entries);

    assert_eq!(outcome.normalized, NORMALIZE_APPLY_MAX);
    assert_eq!(batch(&conn, 100).total_unnormalized, 0);
}

#[test]
fn normalizing_the_same_import_twice_creates_two_distillations() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    import(
        &conn,
        "mem_raw",
        "raw",
        "chat_import",
        "2026-01-01T00:00:00Z",
    );

    let first = apply(&conn, vec![entry("mem_raw")]).results[0]
        .normalized_id
        .clone();
    let second = apply(&conn, vec![entry("mem_raw")]).results[0]
        .normalized_id
        .clone();

    // Ids carry no content determinism, so a second pass is a second memory
    // rather than an overwrite. Pinning it so the behaviour is a decision
    // rather than an accident.
    assert_ne!(first, second);
}
