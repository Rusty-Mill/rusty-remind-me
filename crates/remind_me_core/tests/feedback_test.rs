//! Coverage for `remind_me_feedback`.

use remind_me_core::db::feedback::Feedback;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::vitality::{
    apply_feedback_adjustment, contextual_feedback_adjustment, record_feedback, tokenize_query,
    FeedbackSignal, BASE_WEIGHT_MAX, BASE_WEIGHT_MIN, FEEDBACK_ADJUSTMENT_CAP, FEEDBACK_MAGNITUDE,
};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput, MemorySearchResult};

fn add(store: &Store<'_>) -> String {
    queries::add_memory(
        store,
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

fn base_weight(store: &Store<'_>, id: &str) -> f64 {
    testing::memory_f64(store, id, "base_weight")
        .unwrap()
        .unwrap()
}

fn feedback_rows(store: &Store<'_>, id: &str) -> i64 {
    Feedback::new(store).events(id).unwrap().len() as i64
}

#[test]
fn helpful_without_a_query_raises_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(&store, &id, FeedbackSignal::Helpful, None).unwrap();

    assert!(
        (base_weight(&store, &id) - (1.0 + FEEDBACK_MAGNITUDE)).abs() < 1e-9,
        "got {}",
        base_weight(&store, &id)
    );
}

#[test]
fn unhelpful_without_a_query_lowers_the_weight_globally() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(&store, &id, FeedbackSignal::Unhelpful, None).unwrap();

    assert!((base_weight(&store, &id) - (1.0 - FEEDBACK_MAGNITUDE)).abs() < 1e-9);
}

#[test]
fn global_feedback_writes_no_row() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(&store, &id, FeedbackSignal::Helpful, None).unwrap();

    assert_eq!(
        feedback_rows(&store, &id),
        0,
        "a global judgement lives in base_weight, not the log"
    );
}

#[test]
fn contextual_feedback_logs_a_row_and_leaves_the_weight_alone() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    let before = base_weight(&store, &id);

    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what is my favourite editor"),
    )
    .unwrap();

    assert_eq!(feedback_rows(&store, &id), 1);
    assert!(
        (base_weight(&store, &id) - before).abs() < 1e-9,
        "a memory can be wrong for one question and right for another; \
         contextual feedback must not demote it everywhere"
    );
}

#[test]
fn contextual_feedback_stores_normalised_query_tokens() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(
        &store,
        &id,
        FeedbackSignal::Helpful,
        Some("What IS my Editor?"),
    )
    .unwrap();

    let query = testing::feedback_queries(&store, &id).unwrap().remove(0);
    let tokens = Feedback::new(&store)
        .events(&id)
        .unwrap()
        .remove(0)
        .query_tokens;

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
    let store = db.store();
    let id = add(&store);

    for _ in 0..3 {
        record_feedback(&store, &id, FeedbackSignal::Helpful, Some("same question")).unwrap();
    }

    assert_eq!(
        feedback_rows(&store, &id),
        3,
        "the log is append-only; identical events are separate observations"
    );
}

#[test]
fn a_blank_query_is_treated_as_global() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(&store, &id, FeedbackSignal::Helpful, Some("   ")).unwrap();

    assert_eq!(feedback_rows(&store, &id), 0);
    assert!(
        base_weight(&store, &id) > 1.0,
        "should have taken the global path"
    );
}

#[test]
fn repeated_helpful_feedback_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    for _ in 0..50 {
        record_feedback(&store, &id, FeedbackSignal::Helpful, None).unwrap();
    }

    assert!(
        (base_weight(&store, &id) - BASE_WEIGHT_MAX).abs() < 1e-9,
        "unbounded growth would let one memory dominate every search, got {}",
        base_weight(&store, &id)
    );
}

#[test]
fn repeated_unhelpful_feedback_is_floored() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    for _ in 0..100 {
        record_feedback(&store, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    assert!(
        (base_weight(&store, &id) - BASE_WEIGHT_MIN).abs() < 1e-9,
        "got {}",
        base_weight(&store, &id)
    );
}

#[test]
fn the_weight_floor_keeps_a_downvoted_memory_above_dormancy() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    for _ in 0..100 {
        record_feedback(&store, &id, FeedbackSignal::Unhelpful, None).unwrap();
    }

    let status: String = testing::memory_text(&store, &id, "status")
        .unwrap()
        .unwrap();
    // base_weight floors at 0.1, which is above VITALITY_FLOOR of 0.05, so it
    // stays active — pinning that rather than assuming it flips.
    assert_eq!(status, "active");
}

#[test]
fn feedback_never_touches_access_count() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    record_feedback(&store, &id, FeedbackSignal::Helpful, None).unwrap();
    record_feedback(&store, &id, FeedbackSignal::Unhelpful, Some("a query")).unwrap();

    let count: i64 = testing::memory_i64(&store, &id, "access_count")
        .unwrap()
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
        record_feedback(&db.store(), "mem_nope", FeedbackSignal::Helpful, None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn deleting_a_memory_removes_its_feedback() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(&store, &id, FeedbackSignal::Helpful, Some("a query")).unwrap();
    assert_eq!(feedback_rows(&store, &id), 1);

    queries::delete_memory(&store, &id).unwrap();

    // There is no foreign key here — the reference omits it so sync can deliver
    // rows out of order — so this relies on delete_memory cleaning up itself.
    assert_eq!(feedback_rows(&store, &id), 0);
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
    let store = db.store();
    let id = add(&store);

    assert_eq!(
        contextual_feedback_adjustment(&store, &id, "some query").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_zero_for_an_unknown_memory() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    assert_eq!(
        contextual_feedback_adjustment(&store, "mem_nope", "any query").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_positive_for_a_similar_helpful_query() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Helpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let adjustment =
        contextual_feedback_adjustment(&store, &id, "vpn configuration settings").unwrap();
    assert!(adjustment > 0.0, "got {adjustment}");
}

#[test]
fn contextual_feedback_adjustment_is_negative_for_a_similar_unhelpful_query() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let adjustment =
        contextual_feedback_adjustment(&store, &id, "vpn configuration settings").unwrap();
    assert!(adjustment < 0.0, "got {adjustment}");
}

#[test]
fn contextual_feedback_adjustment_ignores_a_query_below_the_similarity_threshold() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what's my favorite editor"),
    )
    .unwrap();

    // The issue's headline case: a genuinely different question about the
    // same memory must not inherit feedback from an unrelated one.
    assert_eq!(
        contextual_feedback_adjustment(&store, &id, "what IDE did I mention last year").unwrap(),
        0.0
    );
}

#[test]
fn contextual_feedback_adjustment_is_capped_in_either_direction() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    // Three identical-query events at FEEDBACK_MAGNITUDE (0.15) and
    // similarity 1.0 sum to 0.45, past the 0.4 cap.
    for _ in 0..3 {
        record_feedback(&store, &id, FeedbackSignal::Helpful, Some("same question")).unwrap();
    }

    let adjustment = contextual_feedback_adjustment(&store, &id, "same question").unwrap();
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
            project: None,
            session_id: None,
            git_remote: None,
            git_branch: None,
            git_sha: None,
            cwd: None,
            valid_from: None,
            valid_until: None,
            confidence: 1.0,
            verified_at: None,
            outcome: None,
            written_by: "unknown".to_string(),
            capture_method: "manual".to_string(),
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
    let store = db.store();

    assert!(apply_feedback_adjustment(&store, "some query", vec![])
        .unwrap()
        .is_empty());
}

#[test]
fn apply_feedback_adjustment_is_a_noop_for_an_empty_query() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);

    let results = apply_feedback_adjustment(&store, "", vec![result(&id, 0.5)]).unwrap();
    assert_eq!(results[0].score, 0.5);
    assert!(results[0].feedback_adjustment.is_none());
}

#[test]
fn apply_feedback_adjustment_leaves_a_result_untouched_without_matching_feedback() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store);
    let b = add(&store);

    let results =
        apply_feedback_adjustment(&store, "some query", vec![result(&a, 0.5), result(&b, 0.3)])
            .unwrap();

    assert_eq!(results[0].score, 0.5);
    assert_eq!(results[1].score, 0.3);
    assert!(results[0].feedback_adjustment.is_none());
    assert!(results[1].feedback_adjustment.is_none());
}

#[test]
fn apply_feedback_adjustment_boosts_a_helpful_match_and_records_the_adjustment() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Helpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let results =
        apply_feedback_adjustment(&store, "vpn configuration settings", vec![result(&id, 0.5)])
            .unwrap();

    assert!(results[0].score > 0.5, "got {}", results[0].score);
    assert!(results[0].feedback_adjustment.unwrap() > 0.0);
}

#[test]
fn apply_feedback_adjustment_demotes_an_unhelpful_match() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let results =
        apply_feedback_adjustment(&store, "vpn configuration settings", vec![result(&id, 0.5)])
            .unwrap();

    assert!(results[0].score < 0.5, "got {}", results[0].score);
}

#[test]
fn apply_feedback_adjustment_can_promote_a_lower_ranked_result_above_a_higher_ranked_one() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let helped = add(&store);
    let plain = add(&store);
    for _ in 0..10 {
        record_feedback(
            &store,
            &helped,
            FeedbackSignal::Helpful,
            Some("vpn configuration settings"),
        )
        .unwrap();
    }

    // helped's score is boosted by the 40% cap: 0.5 * 1.4 = 0.7 > plain's
    // untouched 0.6.
    let results = apply_feedback_adjustment(
        &store,
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
    let store = db.store();
    let id = add(&store);
    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("what's my favorite editor"),
    )
    .unwrap();

    let results = apply_feedback_adjustment(
        &store,
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
        include_expired: true,
        min_confidence: 0.0,
    }
}

#[test]
fn search_memories_demotes_a_result_with_similar_unhelpful_feedback() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = queries::add_memory(
        &store,
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

    let before = queries::search_memories(&store, &search_input("vpn configuration settings"))
        .unwrap()
        .into_iter()
        .find(|r| r.memory.id == id)
        .unwrap()
        .score;

    record_feedback(
        &store,
        &id,
        FeedbackSignal::Unhelpful,
        Some("vpn configuration settings"),
    )
    .unwrap();

    let after = queries::search_memories(&store, &search_input("vpn configuration settings"))
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
