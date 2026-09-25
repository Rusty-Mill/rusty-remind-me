//! Coverage for `remind_me_list` / `remind_me_update` / `remind_me_delete`.

use remind_me_core::db::queries;
use remind_me_core::{
    Database, MemoryAddInput, MemoryListInput, MemorySearchInput, MemoryUpdateInput, UpdateOutcome,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, category: &str, source: &str, tags: &[&str]) -> String {
    let input = MemoryAddInput {
        sensitive: false,
        content: content.to_string(),
        category: category.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        source: source.to_string(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
    };
    queries::add_memory(conn, input).expect("add failed").id
}

fn search(conn: &Connection, query: &str) -> Vec<String> {
    let input = MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
        query: query.to_string(),
        category: None,
        tags: None,
        limit: 50,
        token_budget: 100_000,
        response_format: Default::default(),
        include_dormant: true,
        min_vitality: 0.0,
        verbose: false,
        expand_entities: false,
        include_neighbors: false,
        expand_co_retrieval: false,
        bootstrap: false,
    };
    queries::search_memories(conn, &input)
        .expect("search failed")
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

fn list(conn: &Connection, input: MemoryListInput) -> (Vec<String>, usize) {
    let page = queries::list_memories(conn, &input).expect("list failed");
    (
        page.memories.into_iter().map(|m| m.id).collect(),
        page.total,
    )
}

#[test]
fn list_returns_empty_on_fresh_database() {
    let db = Database::open_in_memory().unwrap();
    let (ids, total) = list(&db.conn(), MemoryListInput::default());
    assert!(ids.is_empty());
    assert_eq!(total, 0);
}

#[test]
fn list_filters_by_category_and_source() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn, "alpha", "fact", "manual", &[]);
    add(&conn, "beta", "decision", "manual", &[]);
    add(&conn, "gamma", "fact", "chat_import", &[]);

    let (ids, total) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            category: Some("fact".into()),
            source: Some("manual".into()),
            limit: 20,
            ..Default::default()
        },
    );
    assert_eq!(ids, vec![a]);
    assert_eq!(total, 1);
}

#[test]
fn list_tag_filter_requires_all_tags() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let both = add(&conn, "has both", "general", "manual", &["rust", "mcp"]);
    add(&conn, "has one", "general", "manual", &["rust"]);
    add(&conn, "has none", "general", "manual", &[]);

    let (ids, total) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            tags: Some(vec!["rust".into(), "mcp".into()]),
            limit: 20,
            ..Default::default()
        },
    );
    assert_eq!(ids, vec![both], "ALL-of semantics, not ANY-of");
    assert_eq!(total, 1);
}

#[test]
fn tag_filtering_tracks_edits_to_a_memory_s_tags() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "retagged", "general", "manual", &["before"]);

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id.clone(),
            content: None,
            category: None,
            tags: Some(vec!["after".into()]),
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    // Filtering reads the normalized `memory_tags` index rather than the JSON
    // column, so this is really asserting the `memories_tags_au` trigger keeps
    // the two in step. Drift here would silently return stale results.
    let (stale, _) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            tags: Some(vec!["before".into()]),
            ..Default::default()
        },
    );
    assert!(stale.is_empty(), "the removed tag must stop matching");

    let (fresh, total) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            tags: Some(vec!["after".into()]),
            ..Default::default()
        },
    );
    assert_eq!(fresh, vec![id], "the added tag must match");
    assert_eq!(total, 1);
}

#[test]
fn list_total_counts_all_matches_not_just_the_page() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("memory {}", i), "general", "manual", &[]);
    }

    let (ids, total) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            limit: 2,
            ..Default::default()
        },
    );
    assert_eq!(ids.len(), 2);
    assert_eq!(total, 5, "total must count beyond the page");
}

#[test]
fn list_pagination_walks_every_row_without_repeats() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("memory {}", i), "general", "manual", &[]);
    }

    let mut seen = Vec::new();
    for offset in (0..6).step_by(2) {
        let (ids, _) = list(
            &conn,
            MemoryListInput {
                include_sensitive: false,
                limit: 2,
                offset,
                ..Default::default()
            },
        );
        seen.extend(ids);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 5, "pages must tile the result set exactly");
}

#[test]
fn list_offset_past_the_end_is_empty_but_reports_total() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "only", "general", "manual", &[]);

    let (ids, total) = list(
        &conn,
        MemoryListInput {
            include_sensitive: false,
            limit: 20,
            offset: 99,
            ..Default::default()
        },
    );
    assert!(ids.is_empty());
    assert_eq!(total, 1);
}

#[test]
fn list_clamps_limit_to_the_reference_bounds() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "only", "general", "manual", &[]);

    let low = queries::list_memories(
        &conn,
        &MemoryListInput {
            include_sensitive: false,
            limit: 0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(low.limit, 1);

    let high = queries::list_memories(
        &conn,
        &MemoryListInput {
            include_sensitive: false,
            limit: 5_000,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(high.limit, 100);
}

#[test]
fn update_changes_only_the_supplied_fields() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "original", "general", "manual", &["keep"]);
    let before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    let outcome = queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id.clone(),
            content: Some("revised".into()),
            category: None,
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    let updated = match outcome {
        UpdateOutcome::Updated(m) => *m,
        other => panic!("expected Updated, got {:?}", other),
    };
    assert_eq!(updated.content, "revised");
    assert_eq!(updated.category, before.category, "category untouched");
    assert_eq!(updated.tags, before.tags, "tags untouched");
    assert_eq!(updated.created_at, before.created_at, "created_at frozen");
    assert!(updated.updated_at >= before.updated_at, "updated_at moves");
}

#[test]
fn update_leaves_decay_and_retrieval_history_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general", "manual", &[]);
    let before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    let outcome = queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id,
            content: None,
            category: Some("decision".into()),
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    let updated = match outcome {
        UpdateOutcome::Updated(m) => *m,
        other => panic!("expected Updated, got {:?}", other),
    };
    assert_eq!(updated.category, "decision");
    assert!(
        (updated.decay_rate - before.decay_rate).abs() < 1e-9,
        "decay_rate is derived from memory_type and owned by reclassify; an \
         earlier version recomputed it from category here, which meant an edit \
         could silently contradict a classification"
    );
    assert!(
        (updated.vitality - before.vitality).abs() < 1e-9,
        "vitality carries accrued history and must not be reset by an edit"
    );
    assert!((updated.base_weight - before.base_weight).abs() < 1e-9);
    assert_eq!(updated.access_count, before.access_count);
}

#[test]
fn update_reports_not_found_and_no_fields_distinctly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general", "manual", &[]);

    let missing = queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: "mem_does_not_exist".into(),
            content: Some("x".into()),
            category: None,
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();
    assert!(matches!(missing, UpdateOutcome::NotFound));

    let empty = queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id,
            content: None,
            category: None,
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();
    assert!(matches!(empty, UpdateOutcome::NoFields));
}

#[test]
fn update_keeps_the_fts_index_consistent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(
        &conn,
        "quokka sightings in tasmania",
        "general",
        "manual",
        &[],
    );
    assert_eq!(search(&conn, "quokka"), vec![id.clone()]);

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id.clone(),
            content: Some("wombat sightings in tasmania".into()),
            category: None,
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    assert!(
        search(&conn, "quokka").is_empty(),
        "stale term must leave the FTS index"
    );
    assert_eq!(
        search(&conn, "wombat"),
        vec![id],
        "new term must be indexed"
    );
}

#[test]
fn delete_removes_the_memory_and_reports_missing_ids() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "ephemeral", "general", "manual", &[]);

    assert!(queries::delete_memory(&conn, &id).unwrap());
    assert!(queries::get_memory_by_id(&conn, &id).unwrap().is_none());
    assert!(
        !queries::delete_memory(&conn, &id).unwrap(),
        "second delete reports nothing removed"
    );
    assert!(!queries::delete_memory(&conn, "mem_never_existed").unwrap());
}

#[test]
fn delete_purges_the_fts_row_and_hides_from_list() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "pangolin research notes", "general", "manual", &[]);
    assert_eq!(search(&conn, "pangolin"), vec![id.clone()]);

    queries::delete_memory(&conn, &id).unwrap();

    assert!(
        search(&conn, "pangolin").is_empty(),
        "DI-01: deleted rows must not linger in the FTS index"
    );
    let (ids, total) = list(&conn, MemoryListInput::default());
    assert!(ids.is_empty());
    assert_eq!(total, 0);
}

#[test]
fn delete_cleans_up_dependent_rows_explicitly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let input = MemoryAddInput {
        sensitive: false,
        content: "linked to an entity".to_string(),
        category: "general".to_string(),
        tags: vec![],
        source: "manual".to_string(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![remind_me_core::EntityInput {
            name: "Tasmania".into(),
            kind: Some("place".into()),
            aliases: vec![],
        }],
    };
    let id = queries::add_memory(&conn, input).unwrap().id;

    // Rows in the two tables that have no foreign key back to `memories`.
    conn.execute(
        "INSERT INTO memory_feedback
            (id, memory_id, query, query_tokens, signal, magnitude, created_at)
         VALUES ('fb_1', ?, 'q', '[]', 'helpful', 1.0, '2026-01-01T00:00:00+00:00')",
        rusqlite::params![id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_associations (memory_id_a, memory_id_b, weight, updated_at)
         VALUES (?, 'mem_other', 1, '2026-01-01T00:00:00+00:00')",
        rusqlite::params![id],
    )
    .unwrap();

    queries::delete_memory(&conn, &id).unwrap();

    // The schema carries no foreign keys on these — the reference omits them so
    // sync can deliver a link before the memory it points at — so cleanup is
    // `delete_memory`'s job, not the database's.
    for (table, column) in [
        ("memory_entities", "memory_id"),
        ("memory_feedback", "memory_id"),
        ("memory_associations", "memory_id_a"),
    ] {
        let left: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {} WHERE {} = ?", table, column),
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "{} rows must be cleaned up on delete", table);
    }

    let entities: i64 = conn
        .query_row("SELECT count(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        entities, 1,
        "the entity itself survives; others may cite it"
    );
}
