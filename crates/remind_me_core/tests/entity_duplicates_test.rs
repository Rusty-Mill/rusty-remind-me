//! Likely duplicate entities against a real store (card #65): the shared
//! memory count counts only live memories, the profile carries the field, and
//! the daemon op returns the report the CLI prints.

use remind_me_core::daemon::ops::{execute, Op};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::entity::{entity_profile, link_memory_entity, upsert_entity};
use remind_me_core::entity_duplicates::{entity_duplicates, DuplicateCandidate, Reason};
use remind_me_core::testing;
use remind_me_core::{Database, EntityInput, MemoryAddInput};

fn add(store: &Store<'_>, content: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn entity(store: &Store<'_>, name: &str, aliases: &[&str]) -> String {
    upsert_entity(
        store,
        &EntityInput {
            name: name.to_string(),
            kind: None,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        },
    )
    .unwrap()
    .id
}

/// A memory linked to every entity in `ids`.
fn mention(store: &Store<'_>, ids: &[&str]) -> String {
    let memory = add(store, "a memory mentioning both");
    for id in ids {
        link_memory_entity(store, &memory, id).unwrap();
    }
    memory
}

#[test]
fn similar_names_are_reported_only_with_two_live_shared_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let bot = entity(&store, "deploybot", &[]);
    let boxed = entity(&store, "deploybox", &[]);

    mention(&store, &[&bot, &boxed]);
    assert!(entity_duplicates(&store).unwrap().is_empty(), "one shared");

    let deleted = mention(&store, &[&bot, &boxed]);
    testing::set_memory_column(&store, &deleted, "deleted_at", "2026-01-02T00:00:00Z").unwrap();
    let superseded = mention(&store, &[&bot, &boxed]);
    testing::set_memory_column(&store, &superseded, "superseded_by", "mem_newer").unwrap();
    assert!(
        entity_duplicates(&store).unwrap().is_empty(),
        "deleted and superseded memories do not count"
    );

    mention(&store, &[&bot, &boxed]);
    let found = entity_duplicates(&store).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].reason, Reason::Similar);
    assert_eq!(found[0].shared_memories, 2);
}

#[test]
fn the_profile_lists_possible_duplicates_and_omits_an_empty_list() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "rusty_mux", &[]);
    entity(&store, "Rusty Mux", &[]);
    entity(&store, "Hobart", &[]);

    let profile = entity_profile(&store, "rusty_mux", 20).unwrap().unwrap();
    let names: Vec<&str> = profile
        .possible_duplicates
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(names, ["Rusty Mux"]);

    let lone = entity_profile(&store, "Hobart", 20).unwrap().unwrap();
    let json = serde_json::to_value(&lone).unwrap();
    assert!(json.get("possible_duplicates").is_none(), "{json}");
}

#[test]
fn adding_the_alias_hides_a_reviewed_pair() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "rusty_mux", &[]);
    entity(&store, "rusty-mux", &[]);
    assert_eq!(entity_duplicates(&store).unwrap().len(), 1);

    entity(&store, "rusty_mux", &["rusty-mux"]);
    assert!(entity_duplicates(&store).unwrap().is_empty());
}

#[test]
fn the_daemon_op_returns_the_report() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "rusty_mux", &[]);
    entity(&store, "rusty-mux", &[]);

    let found: Vec<DuplicateCandidate> = execute(&store, &Op::EntityDuplicates)
        .into_result()
        .unwrap();
    assert_eq!(found, entity_duplicates(&store).unwrap());
    let json = serde_json::to_value(&found).unwrap();
    assert_eq!(json[0]["reason"], "same_key");
    assert_eq!(json[0]["shared_memories"], 0);
    assert_eq!(json[0]["kind_mismatch"], false);
    assert!(json[0]["a"]["id"].as_str() < json[0]["b"]["id"].as_str());
}
