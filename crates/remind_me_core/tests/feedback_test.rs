//! Coverage for `remind_me_feedback`.

use remind_me_core::db::queries;
use remind_me_core::vitality::{
    apply_feedback_adjustment, contextual_feedback_adjustment, record_feedback, tokenize_query,
    FeedbackSignal, BASE_WEIGHT_MAX, BASE_WEIGHT_MIN, FEEDBACK_ADJUSTMENT_CAP, FEEDBACK_MAGNITUDE,
};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput, MemorySearchResult};
use rusqlite::Connection;

fn add(conn: &Connection) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: "a memory".into(),
            // "general" gives a type prior of 1.0 and manual a source prior of
            // 1.0, so base_weight starts at exactly 1.0.
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

fn base_weight(conn: &Connection, id: &str) -> f64 {
    conn.query_row(
        "SELECT base_weight FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

fn feedback_rows(conn: &Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM memory_feedback WHERE memory_id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn helpful_without_a_query_raises_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();

    assert!(
        (base_weight(&conn, &id) - (1.0 + FEEDBACK_MAGNITUDE)).abs() < 1e-9,
        "got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn unhelpful_without_a_query_lowers_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();

    assert!((base_weight(&conn, &id) - (1.0 - FEEDBACK_MAGNITUDE)).abs() < 1e-9);
}

#[test]
fn global_feedback_writes_no_row() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();

    assert_eq!(
        feedback_rows(&conn, &id),
        0,
        "a global judgement lives in base_weight, not the log"
    );
}

#[test]
fn contextual_feedback_logs_a_row_and_leaves_the_weight_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    let before = base_weight(&conn, &id);

    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what is my favourite editor"),
    )
    .unwrap();

    assert_eq!(feedback_rows(&conn, &id), 1);
    assert!(
        (base_weight(&conn, &id) - before).abs() < 1e-9,
        "a memory can be wrong for one question and right for another; \
         contextual feedback must not demote it everywhere"
    );
}

#[test]
fn contextual_feedback_stores_normalised_query_tokens() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Helpful,
        Some("What IS my Editor?"),
    )
    .unwrap();

    let (query, tokens): (String, String) = conn
        .query_row(
            "SELECT query, query_tokens FROM memory_feedback WHERE memory_id = ?",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        query, "What IS my Editor?",
        "the raw query is kept verbatim"
    );
    // Lowercased, sorted, de-duplicated, single characters dropped.
    assert_eq!(tokens, "editor is my what");
}

#[test]
fn repeated_contextual_feedback_appends_rather_than_replacing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..3 {
        record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("same question")).unwrap();
    }

    assert_eq!(
        feedback_rows(&conn, &id),
        3,
        "the log is append-only; identical events are separate observations"
    );
}

#[test]
fn a_blank_query_is_treated_as_global() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("   ")).unwrap();

    assert_eq!(feedback_rows(&conn, &id), 0);
    assert!(
        base_weight(&conn, &id) > 1.0,
        "should have taken the global path"
    );
}

#[test]
fn repeated_helpful_feedback_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..50 {
        record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();
    }

    assert!(
        (base_weight(&conn, &id) - BASE_WEIGHT_MAX).abs() < 1e-9,
        "unbounded growth would let one memory dominate every search, got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn repeated_unhelpful_feedback_is_floored() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..100 {
        record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    assert!(
        (base_weight(&conn, &id) - BASE_WEIGHT_MIN).abs() < 1e-9,
        "got {}",
        base_weight(&conn, &id)
    );
}

#[test]
fn the_weight_floor_keeps_a_downvoted_memory_above_dormancy() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    for _ in 0..100 {
        record_feedback(&conn, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    let status: String = conn
        .query_row(
            "SELECT status FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    // base_weight floors at 0.1, which is above VITALITY_FLOOR of 0.05, so it
    // stays active — pinning that rather than assuming it flips.
    assert_eq!(status, "active");
}

#[test]
fn feedback_never_touches_access_count() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    record_feedback(&conn, &id, FeedbackSignal::Helpful, None).unwrap();
    record_feedback(&conn, &id, FeedbackSignal::Unhelpful, Some("a query")).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT access_count FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "access_count feeds sqrt(n+1); a negative access has no meaning"
    );
}

#[test]
fn an_unknown_memory_reports_not_found() {
    let db = Database::open_in_memory().unwrap();
    assert!(
        record_feedback(&db.conn(), "mem_nope", FeedbackSignal::Helpful, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn deleting_a_memory_removes_its_feedback() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("a query")).unwrap();
    assert_eq!(feedback_rows(&conn, &id), 1);

    queries::delete_memory(&conn, &id).unwrap();

    // There is no foreign key here — the reference omits it so sync can deliver
    // rows out of order — so this relies on delete_memory cleaning up itself.
    assert_eq!(feedback_rows(&conn, &id), 0);
}

#[test]
fn tokenize_drops_single_characters_and_deduplicates() {
    assert_eq!(tokenize_query("a the THE cat"), vec!["cat", "the"]);
    assert!(tokenize_query("? ! .").is_empty());
}

// ---------------------------------------------------------------------------
// contextual_feedback_adjustment (read side, issue #94)
// ---------------------------------------------------------------------------

#[test]
fn contextual_feedback_adjustment_is_zero_with_no_stored_feedback() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    assert_eq!(
        contextual_feedback_adjustment(&conn, &id, "some query").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_zero_for_an_unknown_memory() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    assert_eq!(
        contextual_feedback_adjustment(&conn, "mem_nope", "any query").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_positive_for_a_similar_helpful_query() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Helpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let adjustment =
        contextual_feedback_adjustment(&conn, &id, "vpn configuration settings").unwrap();
    assert!(adjustment > 0.0, "got {adjustment}");
}

#[test]
fn contextual_feedback_adjustment_is_negative_for_a_similar_unhelpful_query() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let adjustment =
        contextual_feedback_adjustment(&conn, &id, "vpn configuration settings").unwrap();
    assert!(adjustment < 0.0, "got {adjustment}");
}

#[test]
fn contextual_feedback_adjustment_ignores_a_query_below_the_similarity_threshold() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what's my favorite editor"),
    )
    .unwrap();

    // The issue's headline case: a genuinely different question about the
    // same memory must not inherit feedback from an unrelated one.
    assert_eq!(
        contextual_feedback_adjustment(&conn, &id, "what IDE did I mention last year").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_capped_in_either_direction() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    // Three identical-query events at FEEDBACK_MAGNITUDE (0.15) and
    // similarity 1.0 sum to 0.45, past the 0.4 cap.
    for _ in 0..3 {
        record_feedback(&conn, &id, FeedbackSignal::Helpful, Some("same question")).unwrap();
    }

    let adjustment = contextual_feedback_adjustment(&conn, &id, "same question").unwrap();
    assert!(
        (adjustment - FEEDBACK_ADJUSTMENT_CAP).abs() < 1e-9,
        "got {adjustment}"
    );
}

// ---------------------------------------------------------------------------
// apply_feedback_adjustment (ranking-time integration point, issue #94)
// ---------------------------------------------------------------------------

fn result(id: &str, score: f64) -> MemorySearchResult {
    MemorySearchResult {
        memory: remind_me_core::Memory {
            id: id.to_string(),
            content: String::new(),
            category: "general".to_string(),
            tags: vec![],
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            created_at: String::new(),
            updated_at: String::new(),
            capture_id: None,
            subject: None,
            predicate: None,
            object: None,
            superseded_by: None,
            decay_rate: 0.0,
            vitality: 0.0,
            base_weight: 0.0,
            access_count: 0,
            accessed_at: String::new(),
            doc_id: None,
            chunk_index: None,
            remind_at: None,
            sensitive: false,
            // Present so a Memory can round-trip to JSON (#198); feedback
            // scoring reads none of them.
            memory_type: None,
            status: None,
            node_id: None,
            client: None,
            source_capture_id: None,
            deleted_at: None,
        },
        score,
        fts_score: Some(score),
        vec_score: None,
        recency_score: None,
        vitality_score: None,
        idf_score: None,
        feedback_adjustment: None,
        rerank_score: None,
    }
}

#[test]
fn apply_feedback_adjustment_is_a_noop_for_an_empty_result_list() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    assert!(apply_feedback_adjustment(&conn, "some query", vec![])
        .unwrap()
        .is_empty());
}

#[test]
fn apply_feedback_adjustment_is_a_noop_for_an_empty_query() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);

    let results = apply_feedback_adjustment(&conn, "", vec![result(&id, 0.5)]).unwrap();
    assert_eq!(results[0].score, 0.5);
    assert!(results[0].feedback_adjustment.is_none());
}

#[test]
fn apply_feedback_adjustment_leaves_a_result_untouched_without_matching_feedback() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn);
    let b = add(&conn);

    let results =
        apply_feedback_adjustment(&conn, "some query", vec![result(&a, 0.5), result(&b, 0.3)])
            .unwrap();

    assert_eq!(results[0].score, 0.5);
    assert_eq!(results[1].score, 0.3);
    assert!(results[0].feedback_adjustment.is_none());
    assert!(results[1].feedback_adjustment.is_none());
}

#[test]
fn apply_feedback_adjustment_boosts_a_helpful_match_and_records_the_adjustment() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Helpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let results =
        apply_feedback_adjustment(&conn, "vpn configuration settings", vec![result(&id, 0.5)])
            .unwrap();

    assert!(results[0].score > 0.5, "got {}", results[0].score);
    assert!(results[0].feedback_adjustment.unwrap() > 0.0);
}

#[test]
fn apply_feedback_adjustment_demotes_an_unhelpful_match() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let results =
        apply_feedback_adjustment(&conn, "vpn configuration settings", vec![result(&id, 0.5)])
            .unwrap();

    assert!(results[0].score < 0.5, "got {}", results[0].score);
}

#[test]
fn apply_feedback_adjustment_can_promote_a_lower_ranked_result_above_a_higher_ranked_one() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let helped = add(&conn);
    let plain = add(&conn);
    for _ in 0..10 {
        record_feedback(
            &conn,
            &helped,
            FeedbackSignal::Helpful,
            Some("vpn configuration settings"),
        )
        .unwrap();
    }

    // helped's score is boosted by the 40% cap: 0.5 * 1.4 = 0.7 > plain's
    // untouched 0.6.
    let results = apply_feedback_adjustment(
        &conn,
        "vpn configuration settings",
        vec![result(&plain, 0.6), result(&helped, 0.5)],
    )
    .unwrap();

    assert_eq!(results[0].memory.id, helped);
    assert_eq!(results[1].memory.id, plain);
}

#[test]
fn apply_feedback_adjustment_ignores_a_dissimilar_past_query_end_to_end() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn);
    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what's my favorite editor"),
    )
    .unwrap();

    let results = apply_feedback_adjustment(
        &conn,
        "what IDE did I mention last year",
        vec![result(&id, 0.5)],
    )
    .unwrap();

    assert_eq!(results[0].score, 0.5);
    assert!(results[0].feedback_adjustment.is_none());
}

// ---------------------------------------------------------------------------
// End-to-end through queries::search_memories
// ---------------------------------------------------------------------------

fn search_input(query: &str) -> MemorySearchInput {
    MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
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
    }
}

#[test]
fn search_memories_demotes_a_result_with_similar_unhelpful_feedback() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = queries::add_memory(
        &conn,
        MemoryAddInput {
            sensitive: false,
            content: "the vpn configuration settings are in the ops wiki".to_string(),
            category: "general".to_string(),
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
    .id;

    let before = queries::search_memories(&conn, &search_input("vpn configuration settings"))
        .unwrap()
        .into_iter()
        .find(|r| r.memory.id == id)
        .unwrap()
        .score;

    record_feedback(
        &conn,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let after = queries::search_memories(&conn, &search_input("vpn configuration settings"))
        .unwrap()
        .into_iter()
        .find(|r| r.memory.id == id)
        .unwrap();

    assert!(
        after.score < before,
        "before={before}, after={}",
        after.score
    );
    assert!(after.feedback_adjustment.unwrap() < 0.0);
}
