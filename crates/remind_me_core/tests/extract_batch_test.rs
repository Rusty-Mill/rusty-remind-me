//! Coverage for `remind_me_extract_batch`.

use remind_me_core::capture::auto_capture;
use remind_me_core::db::queries;
use remind_me_core::{
    AnnotateInput, AutoCaptureInput, Database, EntityInput, ExtractBatchInput, MemoryAddInput,
    MemoryAnnotation, EXTRACT_BATCH_MAX,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
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

fn batch(conn: &Connection, size: usize) -> remind_me_core::ExtractBatchResult {
    queries::unannotated_batch(conn, &ExtractBatchInput { batch_size: size }).unwrap()
}

fn ids(result: &remind_me_core::ExtractBatchResult) -> Vec<String> {
    result.memories.iter().map(|m| m.id.clone()).collect()
}

fn annotate(conn: &Connection, annotation: MemoryAnnotation) {
    queries::annotate_memories(
        conn,
        &AnnotateInput {
            annotations: vec![annotation],
        },
    )
    .unwrap();
}

#[test]
fn a_bare_memory_needs_extraction() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "something unannotated");

    let result = batch(&conn, 20);

    assert_eq!(ids(&result), vec![id]);
    assert_eq!(result.total_unannotated, 1);
}

#[test]
fn a_memory_with_a_triple_is_done() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "has a triple");
    add(&conn, "has nothing");

    annotate(
        &conn,
        MemoryAnnotation {
            memory_id: id.clone(),
            subject: Some("Bailey".into()),
            predicate: Some("prefers".into()),
            object: Some("Rust".into()),
            entities: vec![],
        },
    );

    assert!(!ids(&batch(&conn, 20)).contains(&id));
    assert_eq!(batch(&conn, 20).total_unannotated, 1);
}

#[test]
fn a_memory_with_only_entities_is_also_done() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "has an entity");

    annotate(
        &conn,
        MemoryAnnotation {
            memory_id: id.clone(),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![EntityInput {
                name: "Tasmania".into(),
                kind: None,
                aliases: vec![],
            }],
        },
    );

    // Missing *both* signals is what qualifies. An OR here would keep
    // re-offering work that has already been done.
    assert!(batch(&conn, 20).memories.is_empty());
    assert_eq!(batch(&conn, 20).total_unannotated, 0);
}

#[test]
fn a_partial_triple_still_counts_as_annotated() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "half a triple");

    annotate(
        &conn,
        MemoryAnnotation {
            memory_id: id.clone(),
            subject: Some("Bailey".into()),
            predicate: None,
            object: None,
            entities: vec![],
        },
    );

    // The predicate requires all three to be NULL, so any one of them being
    // set takes the memory out of the backlog.
    assert!(batch(&conn, 20).memories.is_empty());
}

#[test]
fn raw_dialogs_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let capture = auto_capture(
        &conn,
        &AutoCaptureInput {
            conversation: "user: hi\nassistant: hello".into(),
            summary: "we said hello".into(),
            title: String::new(),
            tags: vec![],
            category: "conversation".into(),
            metadata: serde_json::json!({}),
        },
    )
    .unwrap();

    let result = batch(&conn, 20);

    // A raw transcript's facts come out through decompose, not annotation.
    // Without this exclusion every captured conversation would flood the
    // backlog — which is exactly why the dialog half is stored under the
    // 'dialog' category rather than the caller's.
    assert!(!ids(&result).contains(&capture.dialog_id));
    assert_eq!(
        ids(&result),
        vec![capture.summary_id],
        "the summary is still a candidate"
    );
}

#[test]
fn superseded_and_deleted_memories_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let live = add(&conn, "still here");
    let old = add(&conn, "replaced");
    let gone = add(&conn, "deleted");
    conn.execute(
        "UPDATE memories SET superseded_by = ? WHERE id = ?",
        rusqlite::params![live, old],
    )
    .unwrap();
    queries::delete_memory(&conn, &gone).unwrap();

    let result = batch(&conn, 20);

    assert_eq!(ids(&result), vec![live]);
    assert_eq!(result.total_unannotated, 1);
}

#[test]
fn the_batch_reports_the_full_backlog_not_just_the_page() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..7 {
        add(&conn, &format!("memory {}", i));
    }

    let result = batch(&conn, 3);

    assert_eq!(result.memories.len(), 3);
    assert_eq!(result.total_unannotated, 7);
}

#[test]
fn the_batch_size_is_clamped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..3 {
        add(&conn, &format!("memory {}", i));
    }

    assert_eq!(batch(&conn, 0).memories.len(), 1, "zero clamps up to 1");
    assert_eq!(batch(&conn, EXTRACT_BATCH_MAX * 10).memories.len(), 3);
}

#[test]
fn the_snippet_is_capped_and_the_fields_come_through() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, &"x".repeat(900));

    let result = batch(&conn, 20);

    assert_eq!(result.memories[0].content_snippet.chars().count(), 500);
    assert_eq!(result.memories[0].category, "general");
    assert_eq!(result.memories[0].tags, vec!["tagged".to_string()]);
    // Carried here but not by the reclassify batch — an extractor benefits from
    // knowing what a memory has already been classified as.
    assert_eq!(result.memories[0].memory_type, "unclassified");
}

#[test]
fn a_multibyte_snippet_boundary_does_not_panic() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, &"é".repeat(900));

    assert_eq!(
        batch(&conn, 20).memories[0].content_snippet.chars().count(),
        500
    );
}

#[test]
fn annotating_removes_a_memory_from_the_backlog() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "pending");
    assert_eq!(batch(&conn, 20).total_unannotated, 1);

    annotate(
        &conn,
        MemoryAnnotation {
            memory_id: id,
            subject: Some("Bailey".into()),
            predicate: Some("likes".into()),
            object: Some("quokkas".into()),
            entities: vec![],
        },
    );

    // The loop has to converge: work done must stop being offered.
    assert_eq!(batch(&conn, 20).total_unannotated, 0);
}
