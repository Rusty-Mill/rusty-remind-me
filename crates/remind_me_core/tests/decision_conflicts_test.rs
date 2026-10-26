//! The contradiction check on save for decisions (Mill Board #63), and
//! `remind_me_add`'s `supersedes`.

use remind_me_core::contradictions::{check_on_save, check_on_update, save_check};
use remind_me_core::db::queries;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::kinds::StructuredFields;
use remind_me_core::{ConflictReason, Database, EntityInput, MemoryAddInput, MemoryUpdateInput};
use serde_json::json;

fn input(content: &str, entities: &[&str]) -> MemoryAddInput {
    MemoryAddInput {
        content: content.to_string(),
        metadata: json!({ "rationale": "because" }),
        entities: entities
            .iter()
            .map(|name| EntityInput {
                name: name.to_string(),
                kind: None,
                aliases: vec![],
            })
            .collect(),
        extract: false,
        ..Default::default()
    }
}

fn kind(memory_type: &str) -> StructuredFields {
    StructuredFields {
        memory_type: Some(memory_type.to_string()),
        ..Default::default()
    }
}

fn add_as(store: &Store<'_>, add: MemoryAddInput, memory_type: &str) -> String {
    queries::add_memory_with(store, add, &kind(memory_type))
        .unwrap()
        .id
}

fn decision(store: &Store<'_>, content: &str, entities: &[&str]) -> String {
    add_as(store, input(content, entities), "decision")
}

fn ids(store: &Store<'_>, memory_id: &str) -> Vec<String> {
    check_on_save(store, memory_id)
        .contradiction_candidates
        .into_iter()
        .map(|c| c.memory.id)
        .collect()
}

#[test]
fn a_decision_on_the_same_topic_and_entity_is_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = decision(&store, "Use Postgres for the session store", &["Postgres"]);
    let new = decision(&store, "Use Redis for the session store", &["Postgres"]);

    let check = check_on_save(&store, &new);

    assert_eq!(check.contradiction_check_error, None);
    let [conflict] = check.contradiction_candidates.as_slice() else {
        panic!("one conflict, got {:?}", check.contradiction_candidates);
    };
    assert_eq!(conflict.memory.id, old);
    assert_eq!(conflict.reason, ConflictReason::SharedEntity);
    assert!(conflict.score >= 0.3);
    assert_eq!(conflict.shared_entities, vec!["Postgres".to_string()]);
}

#[test]
fn the_same_claim_with_a_different_object_scores_highest_without_an_entity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let triple = |content: &str, object: &str| MemoryAddInput {
        subject: Some("session store".into()),
        predicate: Some("uses".into()),
        object: Some(object.into()),
        ..input(content, &[])
    };
    let old = add_as(&store, triple("We picked Postgres", "Postgres"), "decision");
    let new = add_as(&store, triple("Switching over", "Redis"), "decision");

    let check = check_on_save(&store, &new);

    let [conflict] = check.contradiction_candidates.as_slice() else {
        panic!("one conflict, got {:?}", check.contradiction_candidates);
    };
    assert_eq!(conflict.memory.id, old);
    assert_eq!(conflict.reason, ConflictReason::SameClaim);
    assert_eq!(conflict.score, 1.0);
    // Never superseded by the check itself.
    assert!(queries::get_memory_by_id(&store, &old)
        .unwrap()
        .unwrap()
        .superseded_by
        .is_none());
}

#[test]
fn an_unrelated_decision_is_not_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    decision(&store, "Release every second Tuesday", &["Release train"]);
    let new = decision(&store, "Use Redis for the session store", &["Redis"]);

    assert!(ids(&store, &new).is_empty());
}

#[test]
fn a_shared_entity_on_a_different_topic_is_not_returned() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    decision(
        &store,
        "Back up Postgres nightly to cold storage",
        &["Postgres"],
    );
    let new = decision(&store, "Use Postgres for the session store", &["Postgres"]);

    assert!(ids(&store, &new).is_empty());
}

#[test]
fn only_standing_decisions_are_compared() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // The same topic, but a note rather than a decision.
    add_as(
        &store,
        input("Use Postgres for the session store", &["Postgres"]),
        "fact",
    );
    // The same topic, but reverted.
    let reverted = StructuredFields {
        outcome: Some("reverted".into()),
        ..kind("decision")
    };
    queries::add_memory_with(
        &store,
        input("Use Memcached for the session store", &["Postgres"]),
        &reverted,
    )
    .unwrap();
    let new = decision(&store, "Use Redis for the session store", &["Postgres"]);

    assert!(ids(&store, &new).is_empty());
}

#[test]
fn a_memory_that_is_not_a_decision_is_not_checked() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    decision(&store, "Use Postgres for the session store", &["Postgres"]);
    let note = add_as(
        &store,
        input("Use Redis for the session store", &["Postgres"]),
        "fact",
    );

    assert_eq!(check_on_save(&store, &note), Default::default());
}

#[test]
fn an_update_checks_only_when_content_or_type_changes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    decision(&store, "Use Postgres for the session store", &["Postgres"]);
    let new = decision(&store, "Use Redis for the session store", &["Postgres"]);
    let update =
        |args: serde_json::Value| -> MemoryUpdateInput { serde_json::from_value(args).unwrap() };

    let tags_only = update(json!({ "memory_id": new, "tags": ["infra"] }));
    let content = update(json!({
        "memory_id": new,
        "content": "Use Redis for the session store, finally",
    }));

    assert_eq!(
        check_on_update(&store, &tags_only, &StructuredFields::default()),
        Default::default()
    );
    assert_eq!(
        check_on_update(&store, &content, &StructuredFields::default())
            .contradiction_candidates
            .len(),
        1
    );
}

#[test]
fn a_failed_check_is_reported_and_the_write_stands() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let new = decision(&store, "Use Redis for the session store", &["Redis"]);

    let check = save_check(&new, Err(StoreError::Engine("disk on fire".into())));

    assert!(check.contradiction_candidates.is_empty());
    assert_eq!(
        check.contradiction_check_error.as_deref(),
        Some("engine store: disk on fire")
    );
    assert!(queries::get_memory_by_id(&store, &new).unwrap().is_some());
}

#[test]
fn supersedes_retires_the_named_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = decision(&store, "Use Postgres for the session store", &["Postgres"]);

    let new = add_as(
        &store,
        MemoryAddInput {
            supersedes: vec![old.clone()],
            ..input("Use Redis for the session store", &["Postgres"])
        },
        "decision",
    );

    let old = queries::get_memory_by_id(&store, &old).unwrap().unwrap();
    assert_eq!(old.superseded_by.as_deref(), Some(new.as_str()));
    // Resolved in one call: nothing left to flag.
    assert!(ids(&store, &new).is_empty());
}

#[test]
fn supersedes_naming_a_memory_that_is_not_live_refuses_the_add() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let before = live_count(&store);

    let refused = queries::add_memory_with(
        &store,
        MemoryAddInput {
            supersedes: vec!["mem_missing".into()],
            ..input("Use Redis for the session store", &[])
        },
        &kind("decision"),
    );

    assert!(matches!(refused, Err(StoreError::Invalid(why)) if why.contains("mem_missing")));
    assert_eq!(live_count(&store), before);
}

fn live_count(store: &Store<'_>) -> usize {
    queries::list_memories(store, &Default::default())
        .unwrap()
        .total
}

#[test]
fn a_failure_on_a_later_target_rolls_back_the_whole_add() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = decision(&store, "Use Postgres for the session store", &["Postgres"]);
    let before = live_count(&store);

    // The second retirement finds `old` already retired by the first.
    let refused = queries::add_memory_with(
        &store,
        MemoryAddInput {
            supersedes: vec![old.clone(), old.clone()],
            ..input("Use Redis for the session store", &["Postgres"])
        },
        &kind("decision"),
    );

    assert!(
        matches!(refused, Err(StoreError::Invalid(_))),
        "{refused:?}"
    );
    assert_eq!(live_count(&store), before);
    let old = queries::get_memory_by_id(&store, &old).unwrap().unwrap();
    assert!(
        old.superseded_by.is_none(),
        "the first retirement rolled back"
    );
}

#[test]
fn a_target_already_superseded_refuses_the_add() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = decision(&store, "Use Postgres for the session store", &["Postgres"]);
    let replace = |content: &str| {
        queries::add_memory_with(
            &store,
            MemoryAddInput {
                supersedes: vec![old.clone()],
                ..input(content, &["Postgres"])
            },
            &kind("decision"),
        )
    };
    replace("Use Redis for the session store").unwrap();
    let before = live_count(&store);

    let stale = replace("Use Memcached for the session store");

    assert!(matches!(stale, Err(StoreError::Invalid(why)) if why.contains("already superseded")));
    assert_eq!(live_count(&store), before);
}

#[test]
fn only_decisions_in_the_same_project_are_compared() {
    use remind_me_core::db::derived::Origin;
    use remind_me_core::db::entities::Entities;
    use remind_me_core::db::memories::{Memories, NewMemory};
    const NOW: &str = "2026-10-26T00:00:00+00:00";

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let insert = |id: &str, object: &str, project: Option<&str>| {
        Memories::new(&store)
            .insert(&NewMemory {
                memory_type: "decision".into(),
                subject: Some("session store".into()),
                predicate: Some("uses".into()),
                object: Some(object.into()),
                project: project.map(str::to_string),
                ..NewMemory::new(id, format!("Use {object} for the session store"), NOW)
            })
            .unwrap();
        Entities::new(&store)
            .link(id, "postgres", NOW, Origin::Local)
            .unwrap();
    };
    insert("same", "Postgres", Some("ALPHA"));
    insert("other", "Memcached", Some("beta"));
    insert("unnamed", "Valkey", None);
    insert("saved", "Redis", Some("alpha"));

    assert_eq!(ids(&store, "saved"), vec!["same".to_string()]);
}
