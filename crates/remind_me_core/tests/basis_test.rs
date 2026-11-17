//! Stated facts told apart from inferences (Mill Board #1553):
//! `metadata.basis` and `metadata.premises`.

use remind_me_core::capture::{auto_capture, decompose};
use remind_me_core::contradictions::preferred;
use remind_me_core::db::memories::Memories;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::kinds::{Basis, StructuredFields};
use remind_me_core::promotion::promote;
use remind_me_core::resolve::validate_facts;
use remind_me_core::{
    AtomicFact, AutoCaptureInput, Database, DecomposeInput, MemoryAddInput, PromoteInput, Rung,
};
use serde_json::{json, Value};

fn input(content: &str, metadata: Value) -> MemoryAddInput {
    MemoryAddInput {
        content: content.to_string(),
        metadata,
        extract: false,
        ..Default::default()
    }
}

fn add(store: &Store<'_>, content: &str, metadata: Value) -> String {
    queries::add_memory_with(
        store,
        input(content, metadata),
        &StructuredFields::default(),
    )
    .unwrap()
    .id
}

/// "the service runs on `object`", as a triple.
fn runs_on(store: &Store<'_>, object: &str, metadata: Value) -> String {
    let mut i = input(&format!("the service runs on {object}"), metadata);
    i.subject = Some("service".into());
    i.predicate = Some("runs_on".into());
    i.object = Some(object.into());
    queries::add_memory_with(store, i, &StructuredFields::default())
        .unwrap()
        .id
}

fn superseded_by(store: &Store<'_>, id: &str) -> Option<String> {
    queries::get_memory_by_id(store, id)
        .unwrap()
        .unwrap()
        .superseded_by
}

#[test]
fn each_basis_is_stored() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let said = add(&store, "Nano lives in Leeds", json!({ "basis": "stated" }));
    let guess = add(
        &store,
        "Nano probably works from home",
        json!({ "basis": "inferred", "premises": [said] }),
    );

    let get = |id: &str| queries::get_memory_by_id(&store, id).unwrap().unwrap();
    assert_eq!(Basis::of(&get(&said).metadata), Some(Basis::Stated));
    assert_eq!(Basis::of(&get(&guess).metadata), Some(Basis::Inferred));
}

#[test]
fn an_inference_without_premises_or_an_unknown_basis_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for metadata in [json!({ "basis": "inferred" }), json!({ "basis": "rumour" })] {
        let e = queries::add_memory_with(
            &store,
            input("x", metadata.clone()),
            &StructuredFields::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("basis") || e.contains("premises"),
            "{metadata}: {e}"
        );
    }
}

#[test]
fn a_decomposed_fact_with_a_bad_basis_is_refused() {
    let fact = AtomicFact {
        content: "Nano likes tea".into(),
        memory_type: None,
        extra_tags: vec![],
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        metadata: Some(json!({ "basis": "pattern", "premises": ["mem_a"] })),
    };
    let e = validate_facts(&[fact]).unwrap_err().to_string();
    assert!(e.contains("facts[0]") && e.contains("at least 2"), "{e}");
}

#[test]
fn a_promotion_records_its_sources_as_premises() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "Nano reviews PRs in the morning", json!({}));
    let b = add(&store, "Nano merges after lunch", json!({}));

    for (sources, basis) in [
        (vec![a.clone(), b.clone()], "pattern"),
        (vec![a.clone()], "inferred"),
    ] {
        let rung = if sources.len() == 2 {
            Rung::FactToScenario
        } else {
            Rung::ScenarioToPersona
        };
        let result = promote(
            &store,
            &PromoteInput {
                rung,
                source_ids: sources.clone(),
                content: format!("a {basis} distillation"),
            },
        )
        .unwrap();
        let promoted = queries::get_memory_by_id(&store, &result.promoted_id)
            .unwrap()
            .unwrap();
        assert_eq!(promoted.metadata["basis"], json!(basis));
        assert_eq!(promoted.metadata["premises"], json!(sources));
    }
}

#[test]
fn a_source_named_twice_is_one_premise() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "Nano reviews PRs in the morning", json!({}));

    let result = promote(
        &store,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: vec![a.clone(), a.clone()],
            content: "a distillation of one source".into(),
        },
    )
    .unwrap();
    let promoted = queries::get_memory_by_id(&store, &result.promoted_id)
        .unwrap()
        .unwrap();
    assert_eq!(promoted.metadata["basis"], json!("inferred"));
    assert_eq!(promoted.metadata["premises"], json!([a]));
}

#[test]
fn decompose_refuses_a_bad_basis_and_writes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let captured = capture(&store, "aws");
    let good = runs_on_fact("aws", json!({}));
    let bad = runs_on_fact("gcp", json!({ "basis": "inferred" }));

    let e = decompose(
        &store,
        &DecomposeInput {
            capture_id: captured.clone(),
            facts: vec![good, bad],
            capture_revision: None,
        },
    )
    .unwrap_err()
    .to_string();

    assert!(e.contains("facts[1]") && e.contains("at least 1"), "{e}");
    let live = Memories::new(&store).all_live().unwrap();
    assert!(
        live.iter().all(|m| m.subject.is_none()),
        "a fact was written"
    );
}

fn capture(store: &Store<'_>, object: &str) -> String {
    auto_capture(
        store,
        &AutoCaptureInput {
            conversation: format!("user: where does it run\nassistant: {object}"),
            summary: format!("it runs on {object}"),
            title: String::new(),
            tags: vec![],
            category: "conversation".into(),
            metadata: json!({}),
            project: None,
        },
    )
    .unwrap()
    .capture_id
}

fn runs_on_fact(object: &str, metadata: Value) -> AtomicFact {
    AtomicFact {
        content: format!("the service runs on {object}"),
        memory_type: None,
        extra_tags: vec![],
        subject: Some("service".into()),
        predicate: Some("runs_on".into()),
        object: Some(object.into()),
        entities: vec![],
        metadata: Some(metadata),
    }
}

/// Decompose one fact "the service runs on `object`" from a fresh capture:
/// the path that retires contradicted triples.
fn decomposed(store: &Store<'_>, object: &str, metadata: Value) -> String {
    let result = decompose(
        store,
        &DecomposeInput {
            capture_id: capture(store, object),
            facts: vec![runs_on_fact(object, metadata)],
            capture_revision: None,
        },
    )
    .unwrap()
    .unwrap();
    result.fact_ids[0].clone()
}

#[test]
fn an_inference_does_not_retire_a_statement() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let said = decomposed(&store, "aws", json!({ "basis": "stated" }));
    let guess = decomposed(
        &store,
        "gcp",
        json!({ "basis": "inferred", "premises": [said] }),
    );

    assert_eq!(superseded_by(&store, &said), None);
    let get = |id: &str| queries::get_memory_by_id(&store, id).unwrap().unwrap();
    assert_eq!(preferred(&get(&said), &get(&guess)).unwrap().id, said);
}

#[test]
fn a_new_statement_still_retires_an_old_one() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = decomposed(&store, "aws", json!({ "basis": "stated" }));
    let new = decomposed(&store, "gcp", json!({}));

    assert_eq!(superseded_by(&store, &old), Some(new));
}

#[test]
fn stated_wins_a_conflict_over_higher_confidence() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let guess_meta = json!({ "basis": "inferred", "premises": ["mem_x"] });
    let guess = runs_on(&store, "gcp", guess_meta);
    let said = queries::add_memory_with(
        &store,
        {
            let mut i = input("the service runs on aws", json!({ "basis": "stated" }));
            i.subject = Some("service".into());
            i.predicate = Some("runs_on".into());
            i.object = Some("aws".into());
            i
        },
        &StructuredFields {
            confidence: Some(0.4),
            ..Default::default()
        },
    )
    .unwrap()
    .id;

    let get = |id: &str| queries::get_memory_by_id(&store, id).unwrap().unwrap();
    assert_eq!(preferred(&get(&guess), &get(&said)).unwrap().id, said);
}
