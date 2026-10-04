//! Wave 1A: typed kinds, outcome tracking, and validity-aware ranking, on the
//! engine store.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::capture::{auto_capture, decompose};
use remind_me_core::contradictions::candidates;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::promotions::Promotions;
use remind_me_core::db::queries;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::kinds::StructuredFields;
use remind_me_core::promotion::{promotion_candidates, provenance};
use remind_me_core::resolve::{resolve_memory, validate_facts, validate_reclassify};
use remind_me_core::testing;
use remind_me_core::{
    AnnotateInput, AtomicFact, AutoCaptureInput, Database, DecomposeInput, EntityInput,
    MemoryAddInput, MemoryAnnotation, MemoryClassification, MemorySearchInput, MemoryUpdateInput,
    Rung, UpdateOutcome, SCENARIO_CATEGORY,
};
use serde_json::{json, Value};

fn input(content: &str, metadata: Value) -> MemoryAddInput {
    MemoryAddInput {
        content: content.to_string(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata,
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        sensitive: false,
        ..Default::default()
    }
}

fn fields(v: Value) -> StructuredFields {
    serde_json::from_value(v).unwrap()
}

fn add(store: &Store<'_>, content: &str, f: Value, metadata: Value) -> String {
    queries::add_memory_with(store, input(content, metadata), &fields(f))
        .unwrap()
        .id
}

fn decision(store: &Store<'_>, content: &str) -> String {
    add(
        store,
        content,
        json!({"memory_type": "decision"}),
        json!({"rationale": "because"}),
    )
}

fn update(memory_id: &str) -> MemoryUpdateInput {
    MemoryUpdateInput {
        memory_id: memory_id.to_string(),
        content: None,
        category: None,
        tags: None,
        metadata: None,
        sensitive: None,
        clear_superseded: false,
    }
}

fn count(store: &Store<'_>) -> i64 {
    testing::count(store, testing::Table::Memories).unwrap()
}

fn invalid(err: StoreError) -> String {
    match err {
        StoreError::Invalid(why) => why,
        other => panic!("expected Invalid, got {other}"),
    }
}

#[test]
fn add_stores_the_structured_fields() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(
        &store,
        "use the engine",
        json!({
            "memory_type": "decision", "confidence": 0.6,
            "valid_from": "2026-01-01T00:00:00Z", "valid_until": "2099-01-01T00:00:00Z",
            "outcome": "done"
        }),
        json!({"rationale": "one store"}),
    );
    let m = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert_eq!(m.memory_type.as_deref(), Some("decision"));
    assert_eq!(m.confidence, 0.6);
    assert_eq!(m.valid_until.as_deref(), Some("2099-01-01T00:00:00Z"));
    assert_eq!(m.outcome.as_deref(), Some("done"));
    assert_eq!(m.decay_rate, 0.02, "the kind sets the decay rate");
}

#[test]
fn a_malformed_kind_is_rejected_and_not_stored() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let cases = [
        (json!({"memory_type": "decision"}), json!({}), "rationale"),
        (json!({"memory_type": "bogus"}), json!({}), "bogus"),
        (json!({"confidence": 1.5}), json!({}), "confidence"),
        (json!({"valid_until": "soon"}), json!({}), "valid_until"),
        (
            json!({"memory_type": "fact", "outcome": "done"}),
            json!({}),
            "outcome",
        ),
        (
            json!({"memory_type": "action_item"}),
            json!({"due": "tomorrow"}),
            "due",
        ),
    ];
    for (f, metadata, named) in cases {
        let err = queries::add_memory_with(&store, input("x", metadata), &fields(f)).unwrap_err();
        assert!(invalid(err).contains(named), "should name {named}");
    }
    assert_eq!(count(&store), 0, "nothing stored");
}

#[test]
fn update_validates_against_the_stored_kind_and_metadata() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(&store, "a fact", json!({"memory_type": "fact"}), json!({}));
    let err =
        queries::update_memory_with(&store, &update(&fact), &fields(json!({"outcome": "done"})))
            .unwrap_err();
    assert!(invalid(err).contains("outcome"));

    // Becoming a decision needs a rationale, from this edit or the stored one.
    let err = queries::update_memory_with(
        &store,
        &update(&fact),
        &fields(json!({"memory_type": "decision"})),
    )
    .unwrap_err();
    assert!(invalid(err).contains("rationale"));

    let mut with_rationale = update(&fact);
    with_rationale.metadata = Some(json!({"rationale": "why"}));
    let outcome = queries::update_memory_with(
        &store,
        &with_rationale,
        &fields(json!({"memory_type": "decision", "confidence": 0.5, "outcome": "done"})),
    )
    .unwrap();
    let UpdateOutcome::Updated(m) = outcome else {
        panic!("expected an update");
    };
    assert_eq!(m.memory_type.as_deref(), Some("decision"));
    assert_eq!(m.confidence, 0.5);
    assert_eq!(m.outcome.as_deref(), Some("done"));
    assert_eq!(m.decay_rate, 0.02);
}

#[test]
fn an_update_of_only_structured_fields_is_not_an_empty_update() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "note", json!({}), json!({}));
    let out =
        queries::update_memory_with(&store, &update(&id), &fields(json!({"confidence": 0.2})))
            .unwrap();
    assert!(matches!(out, UpdateOutcome::Updated(_)));
    let out = queries::update_memory_with(&store, &update(&id), &fields(json!({}))).unwrap();
    assert!(matches!(out, UpdateOutcome::NoFields));
}

#[test]
fn reverted_halves_the_weight_and_leaves_a_resolve_revision() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = decision(&store, "adopt tool X");
    let before = testing::memory_f64(&store, &id, "base_weight")
        .unwrap()
        .unwrap();

    let m = resolve_memory(&store, &id, "reverted", Some("it broke CI"))
        .unwrap()
        .unwrap();
    assert_eq!(m.outcome.as_deref(), Some("reverted"));
    assert_eq!(m.metadata["outcome_note"], "it broke CI");
    assert_eq!(m.metadata["rationale"], "because", "existing metadata kept");
    let after = testing::memory_f64(&store, &id, "base_weight")
        .unwrap()
        .unwrap();
    assert!((after - before * 0.5).abs() < 1e-9);
    assert!(m.vitality < before);

    let revisions = remind_me_core::history::history(&store, &id, 10).unwrap();
    assert_eq!(revisions[0].revision_reason.as_deref(), Some("resolve"));
}

#[test]
fn done_leaves_the_weight_and_abandoned_demotes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let done = decision(&store, "ship it");
    let dropped = decision(&store, "rewrite it");
    let w = |id: &str| {
        testing::memory_f64(&store, id, "base_weight")
            .unwrap()
            .unwrap()
    };
    let (w_done, w_dropped) = (w(&done), w(&dropped));

    resolve_memory(&store, &done, "done", None).unwrap();
    resolve_memory(&store, &dropped, "abandoned", None).unwrap();

    assert_eq!(w(&done), w_done);
    assert!((w(&dropped) - w_dropped * 0.5).abs() < 1e-9);
    let m = queries::get_memory_by_id(&store, &done).unwrap().unwrap();
    assert!(m.metadata.get("outcome_note").is_none());
}

#[test]
fn resolve_refuses_the_wrong_kind_or_outcome_and_misses_cleanly() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(
        &store,
        "just a fact",
        json!({"memory_type": "fact"}),
        json!({}),
    );
    let err = resolve_memory(&store, &fact, "done", None).unwrap_err();
    assert!(invalid(err).contains("outcome"));
    let d = decision(&store, "d");
    let err = resolve_memory(&store, &d, "maybe", None).unwrap_err();
    assert!(invalid(err).contains("outcome"));
    assert!(resolve_memory(&store, "mem_ghost", "done", None)
        .unwrap()
        .is_none());
}

#[test]
fn reclassify_and_decompose_guards_name_the_field() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let plain = add(&store, "no rationale", json!({}), json!({}));
    let rich = add(
        &store,
        "has rationale",
        json!({}),
        json!({"rationale": "r"}),
    );
    let class = |id: &str, t: &str| MemoryClassification {
        memory_id: id.to_string(),
        memory_type: t.to_string(),
    };

    let err = validate_reclassify(&store, &[class(&plain, "decision")]).unwrap_err();
    assert!(invalid(err).contains("rationale"));
    assert!(validate_reclassify(&store, &[class(&rich, "decision")]).is_ok());
    assert!(validate_reclassify(&store, &[class(&plain, "fact")]).is_ok());
    assert!(validate_reclassify(&store, &[class(&plain, "bogus")]).is_err());
    assert!(validate_reclassify(&store, &[class("mem_ghost", "decision")]).is_ok());

    let fact = |t: Option<&str>, metadata: Option<Value>| AtomicFact {
        content: "f".into(),
        memory_type: t.map(String::from),
        extra_tags: vec![],
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        metadata,
    };
    assert!(validate_facts(&[fact(None, None), fact(Some("fact"), None)]).is_ok());
    let err = validate_facts(&[fact(Some("decision"), None)]).unwrap_err();
    let why = invalid(err);
    assert!(
        why.contains("facts[0]") && why.contains("rationale"),
        "{why}"
    );
    assert!(validate_facts(&[fact(Some("decision"), Some(json!({"rationale": "r"})))]).is_ok());
}

#[test]
fn a_decomposed_fact_keeps_its_kind_metadata() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let cap = auto_capture(
        &store,
        &AutoCaptureInput {
            conversation: "user: pick a db".into(),
            summary: "picking a db".into(),
            title: String::new(),
            tags: vec![],
            category: "conversation".into(),
            metadata: json!({}),
        },
    )
    .unwrap();
    let result = decompose(
        &store,
        &DecomposeInput {
            capture_id: cap.capture_id.clone(),
            facts: vec![AtomicFact {
                content: "use the engine".into(),
                memory_type: Some("decision".into()),
                extra_tags: vec![],
                subject: None,
                predicate: None,
                object: None,
                entities: vec![],
                metadata: Some(json!({"rationale": "one store"})),
            }],
        },
    )
    .unwrap()
    .unwrap();
    let m = queries::get_memory_by_id(&store, &result.fact_ids[0])
        .unwrap()
        .unwrap();
    assert_eq!(m.metadata["rationale"], "one store");
    assert_eq!(m.metadata["source_capture_id"], cap.capture_id);

    // Provenance follows the capture both ways.
    let fact_prov = provenance(&store, &m.id).unwrap();
    assert!(fact_prov.sources.contains(&cap.dialog_id));
    assert!(fact_prov.sources.contains(&cap.summary_id));
    let cap_prov = provenance(&store, &cap.summary_id).unwrap();
    assert_eq!(cap_prov.derived, vec![m.id.clone()]);
    assert!(provenance(&store, "mem_ghost").unwrap().sources.is_empty());
}

fn search(
    store: &Store<'_>,
    query: &str,
    tweak: impl FnOnce(&mut MemorySearchInput),
) -> Vec<String> {
    let mut s = MemorySearchInput {
        query: query.to_string(),
        ..MemorySearchInput::default()
    };
    tweak(&mut s);
    queries::search_memories_with_embedder(store, &s, None)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.content)
        .collect()
}

#[test]
fn an_expired_memory_ranks_last_and_can_be_excluded() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Written first, so a mere recency edge cannot explain the order.
    add(&store, "quokka habitat live", json!({}), json!({}));
    add(
        &store,
        "quokka habitat expired",
        json!({"valid_until": "2020-01-01T00:00:00Z"}),
        json!({}),
    );

    let all = search(&store, "quokka habitat", |_| {});
    assert_eq!(all.len(), 2, "include_expired defaults on");
    assert!(all[1].ends_with("expired"));

    let live_only = search(&store, "quokka habitat", |s| s.include_expired = false);
    assert_eq!(live_only, vec!["quokka habitat live".to_string()]);
}

#[test]
fn confidence_scales_ranking_and_min_confidence_filters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(
        &store,
        "numbat diet shaky",
        json!({"confidence": 0.1}),
        json!({}),
    );
    add(&store, "numbat diet solid", json!({}), json!({}));

    let ranked = search(&store, "numbat diet", |_| {});
    assert!(ranked[0].ends_with("solid"));
    let confident = search(&store, "numbat diet", |s| s.min_confidence = 0.5);
    assert_eq!(confident, vec!["numbat diet solid".to_string()]);
}

fn triple_memory(store: &Store<'_>, content: &str, object: &str, f: Value) -> String {
    let mut i = input(content, json!({}));
    i.subject = Some("service".into());
    i.predicate = Some("runs_on".into());
    i.object = Some(object.into());
    i.entities = vec![EntityInput {
        name: "Platform".into(),
        kind: None,
        aliases: vec![],
    }];
    queries::add_memory_with(store, i, &fields(f)).unwrap().id
}

#[test]
fn contradiction_candidates_put_the_more_trusted_side_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let weak = triple_memory(
        &store,
        "service runs on aws",
        "aws",
        json!({"confidence": 0.4}),
    );
    let strong = triple_memory(
        &store,
        "service runs on gcp",
        "gcp",
        json!({"confidence": 0.9}),
    );

    let page = candidates(&store, 10, None).unwrap();
    assert_eq!(page.candidates.len(), 1);
    let c = &page.candidates[0];
    assert_eq!(c.recommended_keep.as_deref(), Some(strong.as_str()));
    assert_eq!(c.memory_a.id, strong);
    assert_eq!(c.memory_b.id, weak);
}

#[test]
fn contradiction_ties_fall_through_to_verified_then_updated() {
    use remind_me_core::contradictions::preferred;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = triple_memory(&store, "service runs on aws", "aws", json!({}));
    let b = triple_memory(&store, "service runs on gcp", "gcp", json!({}));
    let get = |id: &str| queries::get_memory_by_id(&store, id).unwrap().unwrap();

    // Same confidence, neither verified: the later update wins.
    testing::set_memory_column(&store, &a, "updated_at", "2026-01-01T00:00:00Z").unwrap();
    testing::set_memory_column(&store, &b, "updated_at", "2026-02-01T00:00:00Z").unwrap();
    assert_eq!(preferred(&get(&a), &get(&b)).unwrap().id, b);

    // Verified beats unverified, whatever the update times say.
    testing::set_memory_column(&store, &a, "verified_at", "2026-03-01T00:00:00Z").unwrap();
    assert_eq!(preferred(&get(&a), &get(&b)).unwrap().id, a);

    // Not a triple conflict: no preference.
    let other = add(&store, "unrelated", json!({}), json!({}));
    assert!(preferred(&get(&a), &get(&other)).is_none());
}

#[test]
fn stale_candidates_report_expired_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(
        &store,
        "old promo",
        json!({"valid_until": "2020-01-01T00:00:00Z"}),
        json!({}),
    );
    add(
        &store,
        "far future",
        json!({"valid_until": "2099-01-01T00:00:00Z"}),
        json!({}),
    );
    add(&store, "no window", json!({}), json!({}));

    let found = remind_me_core::code_refs::stale_candidates(&store, 20).unwrap();
    assert_eq!(found.total_candidates, 1);
    assert_eq!(found.candidates[0].memory_id, old);
    assert_eq!(
        found.candidates[0].expired_at.as_deref(),
        Some("2020-01-01T00:00:00Z")
    );
}

#[test]
fn annotate_supersedes_a_contradicted_triple() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = add(&store, "I live in Seattle", json!({}), json!({}));
    let new = add(&store, "I moved to Boston", json!({}), json!({}));
    let annotate = |id: &str, object: &str| {
        queries::annotate_memories(
            &store,
            &AnnotateInput {
                annotations: vec![MemoryAnnotation {
                    memory_id: id.to_string(),
                    subject: Some("user".into()),
                    predicate: Some("lives_in".into()),
                    object: Some(object.into()),
                    entities: vec![],
                }],
            },
        )
        .unwrap()
    };
    assert!(annotate(&old, "Seattle").results[0]
        .superseded_ids
        .is_empty());
    let result = annotate(&new, "Boston");
    assert_eq!(result.results[0].superseded_ids, vec![old.clone()]);
    let m = Memories::new(&store).get_live(&old).unwrap().unwrap();
    assert_eq!(m.superseded_by.as_deref(), Some(new.as_str()));
}

#[test]
fn a_scenario_built_on_a_reverted_decision_is_not_a_persona_candidate() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let now = chrono::Utc::now().to_rfc3339();
    let seed = |id: &str, category: &str, memory_type: &str| {
        Memories::new(&store)
            .insert(&NewMemory {
                category: category.to_string(),
                memory_type: memory_type.to_string(),
                ..NewMemory::new(id, format!("content of {id}"), &now)
            })
            .unwrap();
    };
    seed("fact_ok", "fact", "fact");
    seed("fact_dec", "fact", "decision");
    seed("scn_ok", SCENARIO_CATEGORY, "unclassified");
    seed("scn_bad", SCENARIO_CATEGORY, "unclassified");
    let promotions = Promotions::new(&store);
    promotions
        .record("scn_ok", "fact_ok", "fact_to_scenario", &now)
        .unwrap();
    promotions
        .record("scn_bad", "fact_ok", "fact_to_scenario", &now)
        .unwrap();
    promotions
        .record("scn_bad", "fact_dec", "fact_to_scenario", &now)
        .unwrap();

    let ids = |store: &Store<'_>| -> Vec<String> {
        promotion_candidates(store, Rung::ScenarioToPersona, 20)
            .unwrap()
            .into_iter()
            .flat_map(|c| c.source_ids)
            .collect()
    };
    let mut before = ids(&store);
    before.sort();
    assert_eq!(before, vec!["scn_bad", "scn_ok"]);

    testing::set_memory_column(&store, "fact_dec", "outcome", "reverted").unwrap();
    assert_eq!(ids(&store), vec!["scn_ok"]);

    // Done leaves it grounded.
    testing::set_memory_column(&store, "fact_dec", "outcome", "done").unwrap();
    assert_eq!(ids(&store).len(), 2);
}
