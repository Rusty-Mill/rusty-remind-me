//! Coverage for `remind_me_reclassify` / `remind_me_reclassify_batch`.

use remind_me_core::db::queries;
use remind_me_core::vitality::get_decay_rate;
use remind_me_core::{
    Database, MemoryAddInput, MemoryClassification, MemoryUpdateInput, ReclassifyBatchInput,
    ReclassifyInput, RECLASSIFY_BATCH_MAX, UNCLASSIFIED,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, category: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
            tags: vec!["tagged".into()],
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

fn classify(conn: &Connection, id: &str, memory_type: &str) -> remind_me_core::ReclassifyResult {
    queries::reclassify_memories(
        conn,
        &ReclassifyInput {
            classifications: vec![MemoryClassification {
                memory_id: id.to_string(),
                memory_type: memory_type.to_string(),
            }],
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

fn decay_rate(conn: &Connection, id: &str) -> f64 {
    conn.query_row(
        "SELECT decay_rate FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_new_memory_starts_unclassified() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "fresh", "general");

    assert_eq!(column(&conn, &id, "memory_type"), UNCLASSIFIED);
}

#[test]
fn classifying_sets_the_type_and_its_decay_rate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a decision", "general");

    let outcome = classify(&conn, &id, "decision");

    assert_eq!(outcome.updated, 1);
    assert_eq!(outcome.total, 1);
    assert!(outcome.not_found.is_empty());
    assert_eq!(column(&conn, &id, "memory_type"), "decision");
    assert!((decay_rate(&conn, &id) - get_decay_rate("decision")).abs() < 1e-9);
}

#[test]
fn classifying_is_idempotent_and_overwrites() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general");

    classify(&conn, &id, "decision");
    classify(&conn, &id, "action_item");

    assert_eq!(column(&conn, &id, "memory_type"), "action_item");
    assert!(
        (decay_rate(&conn, &id) - get_decay_rate("action_item")).abs() < 1e-9,
        "the decay rate must follow the latest type"
    );
}

#[test]
fn an_unknown_type_falls_back_to_the_default_rate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general");

    classify(&conn, &id, "something_invented");

    assert_eq!(column(&conn, &id, "memory_type"), "something_invented");
    assert!((decay_rate(&conn, &id) - 0.10).abs() < 1e-9);
}

#[test]
fn classification_leaves_retrieval_history_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general");
    let before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    classify(&conn, &id, "decision");

    let after = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    assert!((after.vitality - before.vitality).abs() < 1e-9);
    assert!((after.base_weight - before.base_weight).abs() < 1e-9);
    assert_eq!(after.access_count, before.access_count);
}

#[test]
fn classification_moves_updated_at_but_not_created_at() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general");
    let before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    classify(&conn, &id, "fact");

    let after = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    assert_eq!(after.created_at, before.created_at);
    assert!(after.updated_at >= before.updated_at);
}

#[test]
fn unknown_ids_are_reported_without_discarding_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let good = add(&conn, "real", "general");

    let outcome = queries::reclassify_memories(
        &conn,
        &ReclassifyInput {
            classifications: vec![
                MemoryClassification {
                    memory_id: "mem_ghost".into(),
                    memory_type: "fact".into(),
                },
                MemoryClassification {
                    memory_id: good.clone(),
                    memory_type: "fact".into(),
                },
            ],
        },
    )
    .unwrap();

    assert_eq!(outcome.updated, 1);
    assert_eq!(outcome.not_found, vec!["mem_ghost".to_string()]);
    assert_eq!(outcome.total, 2);
    assert_eq!(column(&conn, &good, "memory_type"), "fact");
}

#[test]
fn a_full_batch_applies() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let classifications: Vec<MemoryClassification> = (0..RECLASSIFY_BATCH_MAX)
        .map(|i| MemoryClassification {
            memory_id: add(&conn, &format!("memory {}", i), "general"),
            memory_type: "fact".into(),
        })
        .collect();

    let outcome =
        queries::reclassify_memories(&conn, &ReclassifyInput { classifications }).unwrap();
    assert_eq!(outcome.updated, RECLASSIFY_BATCH_MAX);
}

#[test]
fn the_batch_returns_only_unclassified_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let pending = add(&conn, "still pending", "general");
    let done = add(&conn, "already done", "general");
    classify(&conn, &done, "fact");

    let batch =
        queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 20 }).unwrap();

    let ids: Vec<String> = batch.memories.iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, vec![pending]);
    assert_eq!(batch.total_unclassified, 1);
}

#[test]
fn the_batch_reports_the_full_backlog_not_just_the_page() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..7 {
        add(&conn, &format!("memory {}", i), "general");
    }

    let batch =
        queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 3 }).unwrap();

    assert_eq!(batch.memories.len(), 3);
    assert_eq!(
        batch.total_unclassified, 7,
        "callers need the backlog to know whether another round is worth it"
    );
}

#[test]
fn the_batch_clamps_its_size() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..3 {
        add(&conn, &format!("memory {}", i), "general");
    }

    let low = queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 0 }).unwrap();
    assert_eq!(low.memories.len(), 1, "zero clamps up to the minimum of 1");

    let high =
        queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 5_000 }).unwrap();
    assert_eq!(high.memories.len(), 3);
}

#[test]
fn the_batch_snippet_is_capped_and_tags_are_parsed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, &"x".repeat(900), "general");

    let batch =
        queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 20 }).unwrap();

    assert_eq!(batch.memories[0].content_snippet.chars().count(), 500);
    assert_eq!(batch.memories[0].tags, vec!["tagged".to_string()]);
    assert_eq!(batch.memories[0].category, "general");
}

#[test]
fn deleted_memories_are_not_offered_for_classification() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let doomed = add(&conn, "going", "general");
    add(&conn, "staying", "general");
    queries::delete_memory(&conn, &doomed).unwrap();

    let batch =
        queries::unclassified_batch(&conn, &ReclassifyBatchInput { batch_size: 20 }).unwrap();
    assert_eq!(batch.memories.len(), 1);
    assert_eq!(batch.total_unclassified, 1);
}

#[test]
fn editing_a_category_no_longer_contradicts_a_classification() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content", "general");

    classify(&conn, &id, "decision"); // slow decay, 0.02
    let classified_rate = decay_rate(&conn, &id);

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            sensitive: None,
            memory_id: id.clone(),
            content: None,
            // A fast-decaying category. An earlier version of update_memory
            // recomputed decay_rate from this, silently overriding the
            // classification above — two writers, different sources of truth.
            category: Some("action_item".into()),
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    assert_eq!(column(&conn, &id, "category"), "action_item");
    assert!(
        (decay_rate(&conn, &id) - classified_rate).abs() < 1e-9,
        "decay_rate belongs to memory_type; update must not touch it"
    );
}
