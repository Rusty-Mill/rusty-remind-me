//! Coverage for `remind_me_consolidate`'s DB-touching orchestration layer.
//!
//! The pure clustering/merge algorithm ([`find_clusters`], [`pick_canonical`],
//! [`merge_cluster`]) has its own unit tests inline in
//! `remind_me_core::consolidation`, alongside the code they test — this file
//! only covers what actually needs a live store: candidate fetching, category
//! scoping, the `limit` cap, dry-run's no-op guarantee, and the write path a
//! real merge takes.
//!
//! Embeddings are written directly via raw SQL (bypassing the embedder),
//! matching `vectors_test.rs`'s convention — chunk 0 is what
//! `consolidation::consolidate` reads.

use remind_me_core::consolidation::{consolidate, EVIDENCE_KEY};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::kinds::StructuredFields;
use remind_me_core::testing;
use remind_me_core::{ConsolidateInput, Database, MemoryAddInput, MemoryUpdateInput};
use std::collections::HashMap;

fn add_with_vector(store: &Store<'_>, content: &str, category: &str, vector: &[f32]) -> String {
    let id = queries::add_memory(
        store,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
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
    .id;

    let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
    remind_me_core::db::vectors::Vectors::new(store)
        .put(&id, 0, &bytes)
        .unwrap();
    id
}

fn set_vitality_and_access(store: &Store<'_>, id: &str, vitality: f64, access_count: i64) {
    testing::set_memory_column(store, id, "vitality", vitality).unwrap();
    testing::set_memory_column(store, id, "access_count", access_count).unwrap();
}

fn column_opt(store: &Store<'_>, id: &str, name: &str) -> Option<String> {
    testing::memory_text(store, id, name).unwrap()
}

fn column(store: &Store<'_>, id: &str, name: &str) -> String {
    testing::memory_text(store, id, name).unwrap().unwrap()
}

fn default_input() -> ConsolidateInput {
    ConsolidateInput {
        similarity_threshold: 0.85,
        dry_run: true,
        category: None,
        limit: 500,
        summaries: None,
    }
}

#[test]
fn an_empty_store_reports_no_eligible_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let report = consolidate(&store, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 0);
    assert_eq!(report["message"], "No eligible memories found");
}

#[test]
fn dissimilar_memories_do_not_cluster() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add_with_vector(&store, "quokkas on Rottnest Island", "general", &[1.0, 0.0]);
    add_with_vector(
        &store,
        "the deploy window is Tuesdays",
        "general",
        &[0.0, 1.0],
    );

    let report = consolidate(&store, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 0);
    assert_eq!(
        report["message"],
        "No similar memories found above threshold"
    );
}

#[test]
fn dry_run_reports_a_cluster_and_each_members_similarity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &a, 0.9, 5);
    set_vitality_and_access(&store, &b, 0.5, 1);

    let report = consolidate(&store, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["dry_run"], true);
    let cluster = &report["clusters"][0];
    assert_eq!(cluster["canonical"]["id"], a);
    assert_eq!(cluster["cluster_size"], 2);
    assert_eq!(cluster["members"][0]["id"], b);
    assert!(cluster["members"][0]["similarity"].as_f64().unwrap() > 0.9);
}

#[test]
fn dry_run_leaves_the_store_completely_untouched() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &a, 0.9, 5);
    set_vitality_and_access(&store, &b, 0.5, 1);
    let before_a = queries::get_memory_by_id(&store, &a).unwrap().unwrap();
    let before_b = queries::get_memory_by_id(&store, &b).unwrap().unwrap();

    // Even with a summary supplied, dry_run must still win over an intent to merge.
    let mut summaries = HashMap::new();
    summaries.insert(a.clone(), "a summary that must not be applied".to_string());
    consolidate(
        &store,
        &ConsolidateInput {
            dry_run: true,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    let after_a = queries::get_memory_by_id(&store, &a).unwrap().unwrap();
    let after_b = queries::get_memory_by_id(&store, &b).unwrap().unwrap();
    assert_eq!(after_a.content, before_a.content);
    assert_eq!(after_a.access_count, before_a.access_count);
    assert_eq!(after_b.superseded_by, before_b.superseded_by);
    assert!(after_b.superseded_by.is_none());
}

#[test]
fn category_scoping_excludes_memories_outside_the_requested_category() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Two in "wildlife" that should cluster together...
    add_with_vector(&store, "quokkas are marsupials", "wildlife", &[1.0, 0.0]);
    add_with_vector(
        &store,
        "quokkas are small marsupials",
        "wildlife",
        &[0.99, 0.14107],
    );
    // ...and a third, equally similar memory in a different category that scoping must drop.
    add_with_vector(
        &store,
        "quokkas are cute marsupials",
        "general",
        &[0.98, 0.19867],
    );

    let report = consolidate(
        &store,
        &ConsolidateInput {
            category: Some("wildlife".to_string()),
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["clusters"][0]["cluster_size"], 2);
}

#[test]
fn limit_caps_the_candidate_pool_fed_into_clustering() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Twelve mutually-identical vectors: without the cap they would all
    // cluster together as one group of 12.
    for i in 0..12 {
        add_with_vector(
            &store,
            &format!("identical content {i}"),
            "general",
            &[1.0, 0.0],
        );
    }

    let report = consolidate(
        &store,
        &ConsolidateInput {
            limit: 10, // the reference's floor
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(
        report["clusters"][0]["cluster_size"], 10,
        "the candidate pool was capped at `limit` before clustering ever ran"
    );
}

#[test]
fn a_cluster_with_no_summary_is_skipped_not_merged() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &a, 0.9, 5);
    set_vitality_and_access(&store, &b, 0.5, 1);

    let report = consolidate(
        &store,
        &ConsolidateInput {
            dry_run: false,
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["clusters_merged"], 0);
    assert_eq!(report["skipped_no_summary"][0], a);
    assert!(column_opt(&store, &b, "superseded_by").is_none());
}

#[test]
fn merging_combines_content_sums_access_counts_and_supersedes_members() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let canonical = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &canonical, 0.9, 5);
    set_vitality_and_access(&store, &member, 0.5, 3);

    let mut summaries = HashMap::new();
    summaries.insert(canonical.clone(), "the meeting is at 3pm".to_string());
    let report = consolidate(
        &store,
        &ConsolidateInput {
            dry_run: false,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_merged"], 1);
    assert_eq!(report["memories_superseded"], 1);
    assert_eq!(report["canonical_ids"][0], canonical);
    assert_eq!(
        column(&store, &canonical, "content"),
        "the meeting is at 3pm"
    );

    let access_count = testing::memory_i64(&store, &canonical, "access_count")
        .unwrap()
        .unwrap();
    assert_eq!(access_count, 8, "5 (canonical) + 3 (member)");

    assert_eq!(column(&store, &member, "superseded_by"), canonical);
}

#[test]
fn superseded_members_are_excluded_from_a_later_consolidate_pass() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let canonical = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &canonical, 0.9, 5);
    set_vitality_and_access(&store, &member, 0.5, 3);

    let mut summaries = HashMap::new();
    summaries.insert(canonical.clone(), "the meeting is at 3pm".to_string());
    consolidate(
        &store,
        &ConsolidateInput {
            dry_run: false,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    // The member is now superseded; a second pass should no longer see it
    // (or the cluster it used to form) as a candidate at all.
    let report = consolidate(&store, &default_input()).unwrap();
    assert_eq!(
        report["clusters_found"], 0,
        "the superseded member must not resurface as a consolidation candidate"
    );
}

fn merge_into(store: &Store<'_>, canonical: &str, summary: &str) -> serde_json::Value {
    let mut summaries = HashMap::new();
    summaries.insert(canonical.to_string(), summary.to_string());
    consolidate(
        store,
        &ConsolidateInput {
            dry_run: false,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap()
}

#[test]
fn a_merge_records_evidence_and_raises_base_weight() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let canonical = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &canonical, 0.9, 5);
    set_vitality_and_access(&store, &member, 0.5, 3);
    let before = queries::get_memory_by_id(&store, &canonical)
        .unwrap()
        .unwrap();

    merge_into(&store, &canonical, "the meeting is at 3pm");

    let after = queries::get_memory_by_id(&store, &canonical)
        .unwrap()
        .unwrap();
    assert_eq!(after.metadata[EVIDENCE_KEY], 2);
    let want = before.base_weight * (1.0 + 0.1 * 2f64.ln());
    assert!(
        (after.base_weight - want).abs() < 1e-9,
        "{}",
        after.base_weight
    );
}

#[test]
fn evidence_accumulates_across_merges_without_compounding_the_boost() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let canonical = add_with_vector(&store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let first = add_with_vector(&store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&store, &canonical, 0.9, 5);
    set_vitality_and_access(&store, &first, 0.5, 3);
    let original = queries::get_memory_by_id(&store, &canonical)
        .unwrap()
        .unwrap()
        .base_weight;
    merge_into(&store, &canonical, "the meeting is at 3pm");

    let second = add_with_vector(
        &store,
        "3pm is the meeting time",
        "general",
        &[0.98, 0.19867],
    );
    set_vitality_and_access(&store, &second, 0.1, 0);
    let report = merge_into(&store, &canonical, "the meeting is at 3pm");

    assert_eq!(report["canonical_ids"][0], canonical);
    let after = queries::get_memory_by_id(&store, &canonical)
        .unwrap()
        .unwrap();
    assert_eq!(after.metadata[EVIDENCE_KEY], 3);
    let want = original * (1.0 + 0.1 * 3f64.ln());
    assert!(
        (after.base_weight - want).abs() < 1e-9,
        "{}",
        after.base_weight
    );
}

/// A pair that clusters, canonical first (higher vitality).
fn pair(store: &Store<'_>) -> (String, String) {
    let canonical = add_with_vector(store, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(store, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(store, &canonical, 0.9, 5);
    set_vitality_and_access(store, &member, 0.5, 3);
    (canonical, member)
}

fn replace_metadata(store: &Store<'_>, id: &str, metadata: serde_json::Value) {
    queries::update_memory(
        store,
        &MemoryUpdateInput {
            project: None,
            sensitive: None,
            memory_id: id.to_string(),
            content: None,
            category: None,
            tags: None,
            metadata: Some(metadata),
            clear_superseded: false,
        },
    )
    .unwrap();
}

fn get(store: &Store<'_>, id: &str) -> remind_me_core::Memory {
    queries::get_memory_by_id(store, id).unwrap().unwrap()
}

#[test]
fn a_merge_keeps_a_clients_own_evidence_entry() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (canonical, _) = pair(&store);
    replace_metadata(
        &store,
        &canonical,
        serde_json::json!({"evidence": ["doc-1"]}),
    );

    merge_into(&store, &canonical, "the meeting is at 3pm");

    let after = get(&store, &canonical);
    assert_eq!(after.metadata["evidence"], serde_json::json!(["doc-1"]));
    assert_eq!(after.metadata[EVIDENCE_KEY], 2);
}

#[test]
fn an_update_cannot_reset_the_count_so_the_boost_does_not_compound() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (canonical, _) = pair(&store);
    let original = get(&store, &canonical).base_weight;
    merge_into(&store, &canonical, "the meeting is at 3pm");

    // The issue's repro: replace metadata, dropping the count, then merge again.
    replace_metadata(&store, &canonical, serde_json::json!({"note": "edited"}));
    let kept = get(&store, &canonical);
    assert_eq!(kept.metadata[EVIDENCE_KEY], 2);
    assert_eq!(kept.metadata["note"], "edited");
    let second = add_with_vector(
        &store,
        "3pm is the meeting time",
        "general",
        &[0.98, 0.19867],
    );
    set_vitality_and_access(&store, &second, 0.1, 0);
    merge_into(&store, &canonical, "the meeting is at 3pm");

    let after = get(&store, &canonical);
    assert_eq!(after.metadata[EVIDENCE_KEY], 3);
    let want = original * (1.0 + 0.1 * 3f64.ln());
    assert!(
        (after.base_weight - want).abs() < 1e-9,
        "{}",
        after.base_weight
    );
}

#[test]
fn a_client_cannot_seed_or_forge_the_count() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let seeded = queries::add_memory(
        &store,
        MemoryAddInput {
            content: "seeded".into(),
            category: "general".into(),
            source: "manual".into(),
            metadata: serde_json::json!({ EVIDENCE_KEY: 50 }),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(seeded.metadata.get(EVIDENCE_KEY).is_none());

    let (canonical, _) = pair(&store);
    merge_into(&store, &canonical, "the meeting is at 3pm");
    replace_metadata(&store, &canonical, serde_json::json!({ EVIDENCE_KEY: 1 }));
    assert_eq!(get(&store, &canonical).metadata[EVIDENCE_KEY], 2);
}

#[test]
fn a_reverted_decision_adds_no_evidence() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (canonical, _) = pair(&store);
    let reverted = queries::add_memory_with(
        &store,
        MemoryAddInput {
            content: "the meeting is at 3pm, we decided".into(),
            category: "general".into(),
            source: "manual".into(),
            metadata: serde_json::json!({ "rationale": "room free" }),
            ..Default::default()
        },
        &StructuredFields {
            memory_type: Some("decision".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    let bytes: Vec<u8> = [0.98f32, 0.19867]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    remind_me_core::db::vectors::Vectors::new(&store)
        .put(&reverted, 0, &bytes)
        .unwrap();
    set_vitality_and_access(&store, &reverted, 0.1, 0);
    remind_me_core::resolve::resolve_memory(&store, &reverted, "reverted", None)
        .unwrap()
        .unwrap();
    let original = get(&store, &canonical).base_weight;

    let report = merge_into(&store, &canonical, "the meeting is at 3pm");

    assert_eq!(report["memories_superseded"], 2);
    let after = get(&store, &canonical);
    assert_eq!(
        after.metadata[EVIDENCE_KEY], 2,
        "the reverted decision is not evidence"
    );
    let want = original * (1.0 + 0.1 * 2f64.ln());
    assert!(
        (after.base_weight - want).abs() < 1e-9,
        "{}",
        after.base_weight
    );
}
