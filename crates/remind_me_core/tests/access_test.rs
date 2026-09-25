//! Coverage for access recording — the half of the vitality model that was
//! inert until now.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::vitality::{record_accesses, BRIDGE_THRESHOLD};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, category: &str) -> String {
    queries::add_memory(
        conn,
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
        },
    )
    .unwrap()
    .id
}

fn input(query: &str) -> MemorySearchInput {
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

fn search(conn: &Connection, query: &str) -> Vec<String> {
    queries::search_with_expansions(conn, &input(query))
        .unwrap()
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect()
}

fn access_count(conn: &Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT access_count FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

fn accessed_at(conn: &Connection, id: &str) -> String {
    conn.query_row(
        "SELECT accessed_at FROM memories WHERE id = ?",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap()
}

fn backdate(conn: &Connection, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    conn.execute(
        "UPDATE memories SET accessed_at = ?, created_at = ? WHERE id = ?",
        rusqlite::params![when, when, id],
    )
    .unwrap();
}

#[test]
fn retrieval_increments_the_count_and_moves_the_stamp() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "fact");
    backdate(&conn, &id, 30);
    let before = accessed_at(&conn, &id);
    assert_eq!(access_count(&conn, &id), 0);

    search(&conn, "quokka");

    assert_eq!(access_count(&conn, &id), 1);
    assert!(accessed_at(&conn, &id) > before);
}

#[test]
fn repeated_retrieval_keeps_counting() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "fact");

    for _ in 0..5 {
        search(&conn, "quokka");
    }

    assert_eq!(access_count(&conn, &id), 5);
}

#[test]
fn a_memory_in_regular_use_outlives_an_abandoned_one() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let used = add(&conn, "quokka in use", "action_item");
    let abandoned = add(&conn, "quokka abandoned", "action_item");
    // Both written a month ago; at 0.20 decay that is dormant.
    backdate(&conn, &used, 30);
    backdate(&conn, &abandoned, 30);

    // One of them is retrieved today.
    queries::search_with_expansions(
        &conn,
        &MemorySearchInput {
            strategy: Default::default(),
            include_sensitive: false,
            query: "use".into(),
            ..input("use")
        },
    )
    .unwrap();

    let mut live = input("quokka");
    live.include_dormant = false;
    let found: Vec<String> = queries::search_with_expansions(&conn, &live)
        .unwrap()
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    // This is the point of the whole feature: dormancy has to measure time
    // since last *use*, not since writing. Before access recording both of
    // these decayed identically.
    assert_eq!(found, vec![used]);
    assert!(!found.contains(&abandoned));
}

#[test]
fn bridge_protection_becomes_reachable() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "fact");

    for _ in 0..BRIDGE_THRESHOLD {
        search(&conn, "quokka");
    }

    // Nothing could reach the bridge threshold before — the only test for it
    // set the column by hand.
    assert_eq!(access_count(&conn, &id), BRIDGE_THRESHOLD);
}

#[test]
fn the_stored_vitality_reflects_the_new_count() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "general");
    let before: f64 = conn
        .query_row(
            "SELECT vitality FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();

    search(&conn, "quokka");

    let after: f64 = conn
        .query_row(
            "SELECT vitality FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    // Recomputed at zero elapsed days, so it collapses to
    // base_weight * sqrt(count + 1): 1.0 * sqrt(2).
    assert!((after - 2.0_f64.sqrt()).abs() < 1e-9, "got {}", after);
    assert!(after > before);
}

#[test]
fn status_is_maintained() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "general");

    search(&conn, "quokka");

    let status: String = conn
        .query_row(
            "SELECT status FROM memories WHERE id = ?",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    // Nothing wrote this column before; the earlier tests pinned it at
    // "active" by observation rather than because anything maintained it.
    assert_eq!(status, "active");
}

#[test]
fn expansion_results_are_not_recorded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let seed = add(&conn, "quokka sighting", "fact");
    let neighbour = add(&conn, "wholly different wording", "fact");
    // Associate them without either being a search hit for the query below.
    remind_me_core::expansion::record_co_retrieval(&conn, &[seed.clone(), neighbour.clone()])
        .unwrap();

    let mut expanded = input("quokka");
    expanded.expand_co_retrieval = true;
    let result = queries::search_with_expansions(&conn, &expanded).unwrap();
    assert_eq!(result.related_via_co_retrieval.unwrap().len(), 1);

    assert_eq!(access_count(&conn, &seed), 1, "a direct hit is recorded");
    assert_eq!(
        access_count(&conn, &neighbour),
        0,
        "an expansion is a discovery aid, not an answer to the query; \
         recording it would inflate every neighbour on every expanded search"
    );
}

#[test]
fn a_plain_search_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "fact");

    queries::search_memories(&conn, &input("quokka")).unwrap();

    assert_eq!(
        access_count(&conn, &id),
        0,
        "search_memories is a pure read; the write lives in the wrapper"
    );
}

#[test]
fn unknown_ids_are_skipped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let real = add(&conn, "quokka sighting", "fact");

    let updated = record_accesses(&conn, &["mem_ghost".to_string(), real.clone()]).unwrap();

    assert_eq!(updated, 1);
    assert_eq!(access_count(&conn, &real), 1);
}

#[test]
fn recording_nothing_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(record_accesses(&db.conn(), &[]).unwrap(), 0);
}

#[test]
fn a_search_that_matches_nothing_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", "fact");

    assert!(search(&conn, "wombat").is_empty());

    assert_eq!(access_count(&conn, &id), 0);
}
