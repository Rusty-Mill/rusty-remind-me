//! The token-budget envelope on `remind_me_search` (#200).
//!
//! The trimming always happened; nothing reported it. A search that dropped
//! half its results was indistinguishable from one that returned everything,
//! so "this is everything that matched" — the reasonable inference — was
//! silently wrong.
//!
//! These assert the **values**, not that the keys exist. A test that only
//! checked for presence would pass with every field hardcoded to zero, which
//! is precisely the shape of bug being fixed.

use remind_me_core::db::queries;
use remind_me_core::models::MemorySearchInput;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

/// Add a memory whose content is `chars` long, so token estimates
/// (`len / 4`) are predictable.
fn add_sized(conn: &Connection, tag: &str, chars: usize) -> String {
    let body = format!("quokka {tag} {}", "x".repeat(chars.saturating_sub(20)));
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: body,
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

fn search(conn: &Connection, budget: usize) -> remind_me_core::expansion::MemorySearchResponse {
    let input = MemorySearchInput {
        query: "quokka".into(),
        limit: 50,
        token_budget: budget,
        ..Default::default()
    };
    queries::search_with_expansions(conn, &input).unwrap()
}

#[test]
fn an_untrimmed_search_reports_nothing_trimmed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..3 {
        add_sized(&conn, &format!("m{i}"), 40);
    }

    // Generous budget: everything fits.
    let res = search(&conn, 100_000);

    assert_eq!(res.returned, 3);
    assert_eq!(res.total_candidates, 3);
    assert_eq!(res.trimmed, 0, "nothing should have been cut");
    assert_eq!(
        res.memories.len(),
        res.returned,
        "returned must match the list"
    );
    assert!(
        res.tokens_used > 0,
        "tokens were spent, so they must be reported"
    );
    assert_eq!(res.budget, 100_000, "the budget in force is echoed back");
}

#[test]
fn a_trimmed_search_says_how_many_it_dropped() {
    // The whole point. Before this, these two cases produced identical
    // responses apart from the length of `memories`, which a caller has no
    // baseline to compare against.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..8 {
        add_sized(&conn, &format!("m{i}"), 400);
    }

    let untrimmed = search(&conn, 0);
    assert_eq!(untrimmed.trimmed, 0);
    let all = untrimmed.total_candidates;
    assert!(all >= 4, "need several candidates to trim, got {all}");

    // A budget that fits roughly one 400-char memory (~100 tokens).
    let res = search(&conn, 120);

    assert!(
        res.returned < all,
        "the budget should have cut something: returned {} of {all}",
        res.returned
    );
    assert_eq!(
        res.trimmed,
        all - res.returned,
        "trimmed must be the difference, not a flag"
    );
    assert_eq!(
        res.total_candidates, all,
        "candidates counted before the cut"
    );
    assert_eq!(res.memories.len(), res.returned);
    assert!(
        res.tokens_used <= 120 || res.returned == 1,
        "tokens_used must respect the budget unless a single oversized result \
         forced it: {} used against 120",
        res.tokens_used
    );
}

#[test]
fn a_budget_of_zero_means_unlimited_and_still_counts_tokens() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add_sized(&conn, &format!("m{i}"), 200);
    }

    let res = search(&conn, 0);

    assert_eq!(res.returned, res.total_candidates);
    assert_eq!(res.trimmed, 0);
    assert_eq!(res.budget, 0);
    assert!(
        res.tokens_used > 0,
        "\"how big was this response\" is a fair question even when nothing \
         was cut"
    );
}

#[test]
fn an_empty_result_set_reports_zeroes_rather_than_omitting_the_envelope() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add_sized(&conn, "unrelated", 40);

    let input = MemorySearchInput {
        query: "nothingmatchesthis".into(),
        token_budget: 500,
        ..Default::default()
    };
    let res = queries::search_with_expansions(&conn, &input).unwrap();

    assert!(res.memories.is_empty());
    assert_eq!(res.returned, 0);
    assert_eq!(res.total_candidates, 0);
    assert_eq!(res.trimmed, 0);
    assert_eq!(res.tokens_used, 0);
    assert_eq!(res.budget, 500, "the budget still describes what was asked");
}

#[test]
fn the_envelope_reaches_the_serialised_response() {
    // The fields only matter if they reach a client. A struct field that
    // never serialised would satisfy every assertion above.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add_sized(&conn, "m", 40);

    let res = search(&conn, 900);
    let json = serde_json::to_value(&res).unwrap();

    for key in [
        "memories",
        "total_candidates",
        "returned",
        "trimmed",
        "tokens_used",
        "budget",
    ] {
        assert!(
            json.get(key).is_some(),
            "{key} is missing from the serialised search response"
        );
    }
    assert_eq!(json["budget"], 900);
}
