//! Pinnable persona lines (Mill Board #1555, pack D101/D102).
//!
//! A pin records a person's wish, so it must outlive the evidence a
//! promoted line stands on, come first, and never be rewritten by a refresh.
//! Only a person may write a line by hand; an agent cannot slip sourceless
//! statements into the persona.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::context::WRITTEN_BY_ENV;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries::{add_memory, update_memory};
use remind_me_core::db::Store;
use remind_me_core::kinds::PersonaKind;
use remind_me_core::promotion::{
    demoted, persona, persona_card, promote, PromotionError, PERSONA_CARD_MAX,
};
use remind_me_core::{
    Database, MemoryAddInput, MemoryUpdateInput, PromoteInput, Rung, PERSONA_CATEGORY,
    SCENARIO_CATEGORY,
};
use serde_json::{json, Value};
use std::sync::Mutex;

/// Guards `REMIND_ME_WRITTEN_BY` for the tests in this binary that read it.
static ENV: Mutex<()> = Mutex::new(());

fn seed(store: &Store<'_>, id: &str, category: &str, metadata: Value, written_by: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    Memories::new(store)
        .insert(&NewMemory {
            category: category.to_string(),
            metadata,
            written_by: written_by.to_string(),
            ..NewMemory::new(id, format!("line {id}"), &now)
        })
        .unwrap();
}

/// A persona statement promoted from one seeded scenario; returns both ids.
fn promoted(store: &Store<'_>, name: &str) -> (String, String) {
    let scenario = format!("mem_sc_{name}");
    seed(store, &scenario, SCENARIO_CATEGORY, json!({}), "model");
    let result = promote(
        store,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec![scenario.clone()],
            content: format!("{name} is durable"),
            refreshes: None,
        },
    )
    .unwrap();
    (result.promoted_id, scenario)
}

fn set_metadata(store: &Store<'_>, id: &str, metadata: Value) -> remind_me_core::db::Result<()> {
    update_memory(
        store,
        &MemoryUpdateInput {
            memory_id: id.to_string(),
            metadata: Some(metadata),
            ..update_input()
        },
    )
    .map(|_| ())
}

fn update_input() -> MemoryUpdateInput {
    serde_json::from_value(json!({"memory_id": ""})).unwrap()
}

fn supersede(store: &Store<'_>, id: &str) {
    remind_me_core::testing::set_memory_column(store, id, "superseded_by", "mem_newer").unwrap();
}

fn ids(statements: &[remind_me_core::PersonaStatement]) -> Vec<&str> {
    statements.iter().map(|s| s.id.as_str()).collect()
}

fn persona_add(metadata: Value) -> MemoryAddInput {
    MemoryAddInput {
        content: "Prefers terse replies".into(),
        category: PERSONA_CATEGORY.into(),
        metadata,
        ..MemoryAddInput::default()
    }
}

#[test]
fn a_pin_survives_supersession_of_all_its_sources() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (line, scenario) = promoted(&store, "pin");
    set_metadata(&store, &line, json!({"pinned": true, "kind": "preference"})).unwrap();

    supersede(&store, &scenario);

    let standing = persona(&store).unwrap();
    assert_eq!(ids(&standing), vec![line.as_str()]);
    assert!(standing[0].pinned);
    assert_eq!(standing[0].kind, Some(PersonaKind::Preference));
    assert_eq!(standing[0].surviving_sources, 0);
    assert!(demoted(&store).unwrap().is_empty());
}

#[test]
fn unpinning_restores_normal_demotion() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (line, scenario) = promoted(&store, "unpin");
    set_metadata(&store, &line, json!({"pinned": true})).unwrap();
    supersede(&store, &scenario);

    set_metadata(&store, &line, json!({"pinned": false})).unwrap();

    assert!(persona(&store).unwrap().is_empty());
    assert_eq!(ids(&demoted(&store).unwrap()), vec![line.as_str()]);
}

#[test]
fn pinned_lines_come_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (first, _) = promoted(&store, "a");
    let (second, _) = promoted(&store, "b");
    let unpinned_order = ids(&persona(&store).unwrap())
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    // Pin whichever currently ranks last, so the test does not depend on ties.
    let last = unpinned_order.last().unwrap().clone();
    set_metadata(&store, &last, json!({"pinned": true})).unwrap();

    let order = persona(&store).unwrap();
    assert_eq!(order[0].id, last);
    assert!(order[0].pinned);
    assert_eq!(order.len(), 2);
    assert!(ids(&order).contains(&first.as_str()) && ids(&order).contains(&second.as_str()));
}

#[test]
fn the_card_lists_at_most_forty_and_counts_all() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..PERSONA_CARD_MAX + 5 {
        seed(
            &store,
            &format!("mem_pin_{i:02}"),
            PERSONA_CATEGORY,
            json!({"pinned": true}),
            "human",
        );
    }

    let (card, total) = persona_card(&store).unwrap();

    assert_eq!(PERSONA_CARD_MAX, 40);
    assert_eq!(card.len(), PERSONA_CARD_MAX);
    assert_eq!(total, PERSONA_CARD_MAX + 5);
}

#[test]
fn a_sourceless_pin_stands_only_when_a_person_wrote_it() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    seed(
        &store,
        "mem_by_person",
        PERSONA_CATEGORY,
        json!({"pinned": true}),
        "human",
    );
    seed(
        &store,
        "mem_by_model",
        PERSONA_CATEGORY,
        json!({"pinned": true}),
        "model",
    );

    assert_eq!(ids(&persona(&store).unwrap()), vec!["mem_by_person"]);
    assert_eq!(ids(&demoted(&store).unwrap()), vec!["mem_by_model"]);
}

#[test]
fn only_a_person_may_add_a_persona_line_and_only_pinned() {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    test_env::remove_var(WRITTEN_BY_ENV);
    let by_model = add_memory(&store, persona_add(json!({"pinned": true})));
    test_env::set_var(WRITTEN_BY_ENV, "human");
    let unpinned = add_memory(&store, persona_add(json!({})));
    let bad_kind = add_memory(&store, persona_add(json!({"pinned": true, "kind": "mood"})));
    let ok = add_memory(
        &store,
        persona_add(json!({"pinned": true, "kind": "trait"})),
    );
    test_env::remove_var(WRITTEN_BY_ENV);

    let err = by_model.unwrap_err().to_string();
    assert!(err.contains("written_by human"), "{err}");
    assert!(unpinned.unwrap_err().to_string().contains("pinned"));
    assert!(bad_kind.unwrap_err().to_string().contains("kind"));
    let ok = ok.unwrap();
    let standing = persona(&store).unwrap();
    assert_eq!(ids(&standing), vec![ok.id.as_str()]);
    assert_eq!(standing[0].kind, Some(PersonaKind::Trait));
}

#[test]
fn an_unknown_kind_or_a_non_boolean_pin_is_refused_on_update() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (line, _) = promoted(&store, "kinds");

    let kind = set_metadata(&store, &line, json!({"kind": "mood"})).unwrap_err();
    let pin = set_metadata(&store, &line, json!({"pinned": "yes"})).unwrap_err();

    assert!(kind.to_string().contains("kind"), "{kind}");
    assert!(pin.to_string().contains("boolean"), "{pin}");
}

#[test]
fn a_model_cannot_move_its_own_memory_into_the_persona() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    seed(&store, "mem_note", "general", json!({}), "model");

    let err = update_memory(
        &store,
        &MemoryUpdateInput {
            memory_id: "mem_note".into(),
            category: Some(PERSONA_CATEGORY.into()),
            metadata: Some(json!({"pinned": true})),
            ..update_input()
        },
    )
    .map(|_| ())
    .unwrap_err();

    assert!(err.to_string().contains("written_by human"), "{err}");
}

#[test]
fn a_refresh_never_replaces_a_pinned_line() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (line, _) = promoted(&store, "refresh");
    set_metadata(&store, &line, json!({"pinned": true})).unwrap();
    seed(&store, "mem_sc_new", SCENARIO_CATEGORY, json!({}), "model");

    let err = promote(
        &store,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec!["mem_sc_new".into()],
            content: "a rewrite".into(),
            refreshes: Some(line.clone()),
        },
    )
    .unwrap_err();

    assert!(matches!(err, PromotionError::PinnedRefresh(ref id) if *id == line));
    assert!(ids(&persona(&store).unwrap()).contains(&line.as_str()));
}
