//! Raw session dialogs leave the ranking by default (ADR-0029).
//!
//! A captured dialog runs to 200,000 characters and matches almost any query,
//! so it would crowd out the facts drawn from it. These check that it stays
//! reachable: on request, by category, and as a marked fallback when nothing
//! refined matches.

use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::models::MemorySearchInput;
use remind_me_core::{Database, MemoryAddInput};

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.into(),
            category: category.into(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn ids(store: &Store<'_>, input: MemorySearchInput) -> (Vec<String>, bool) {
    let res = queries::search_with_expansions(store, &input).unwrap();
    let ids = res.memories.iter().map(|r| r.memory.id.clone()).collect();
    (ids, res.raw_fallback)
}

fn query(text: &str) -> MemorySearchInput {
    MemorySearchInput {
        query: text.into(),
        ..Default::default()
    }
}

#[test]
fn a_dialog_ranks_only_when_asked_for() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(
        &store,
        "quokka releases merge with merge commits",
        "decision",
    );
    let dialog = add(
        &store,
        "user: how do quokka releases merge? assistant: merge commits",
        "dialog",
    );

    let (found, fallback) = ids(&store, query("quokka"));
    assert_eq!(found, vec![fact.clone()]);
    assert!(!fallback);

    let (found, _) = ids(
        &store,
        MemorySearchInput {
            include_raw: true,
            ..query("quokka")
        },
    );
    assert!(found.contains(&fact) && found.contains(&dialog));

    let (found, fallback) = ids(
        &store,
        MemorySearchInput {
            category: Some("dialog".into()),
            ..query("quokka")
        },
    );
    assert_eq!((found, fallback), (vec![dialog], false));
}

#[test]
fn nothing_refined_falls_back_to_marked_dialogs() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(
        &store,
        "quokka releases merge with merge commits",
        "decision",
    );
    let dialog = add(&store, "user: the wombat build is red", "dialog");

    let (found, fallback) = ids(&store, query("wombat"));
    assert_eq!((found, fallback), (vec![dialog], true));

    let (found, fallback) = ids(&store, query("platypus"));
    assert!(found.is_empty());
    assert!(!fallback, "an empty fallback is not reported as one");
}

#[test]
fn a_session_or_raw_category_turns_raw_ranking_on() {
    let mut input = query("x");
    assert!(!input.ranks_raw());
    input.scope.session_id = Some("s1".into());
    assert!(input.ranks_raw());
    let skeleton = MemorySearchInput {
        category: Some("skeleton".into()),
        ..query("x")
    };
    assert!(skeleton.ranks_raw());
}
