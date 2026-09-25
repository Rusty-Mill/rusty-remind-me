//! Coverage for `remind_me_entity_traverse`.
//!
//! Nothing in this crate writes `entity_relations` yet — the tools that do
//! (`decompose`, the relation half of `annotate`) are still to come — so the
//! edges are inserted directly.

use remind_me_core::entity::{entity_id, resolve_entity, traverse_from_name, upsert_entity};
use remind_me_core::{Database, EntityInput, EntityTraverseInput};
use rusqlite::Connection;

fn entity(conn: &Connection, name: &str, aliases: &[&str]) -> String {
    upsert_entity(
        conn,
        &EntityInput {
            name: name.to_string(),
            kind: Some("person".into()),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        },
    )
    .unwrap()
    .id
}

/// Edges are ordered by `created_at`, so the sequence number keeps the
/// breadth-first order deterministic.
fn relate(conn: &Connection, seq: u32, subject: &str, relation: &str, object: &str) {
    let created = format!("2026-01-01T00:00:{:02}Z", seq);
    conn.execute(
        "INSERT INTO entity_relations (id, subject_entity_id, relation, object_entity_id,
                                       created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            format!("rel_{}", seq),
            subject,
            relation,
            object,
            created,
            created
        ],
    )
    .unwrap();
}

fn traverse(
    conn: &Connection,
    name: &str,
    hops: u32,
) -> remind_me_core::entity::EntityTraverseResult {
    traverse_from_name(
        conn,
        &EntityTraverseInput {
            name: name.to_string(),
            hops,
            relation: None,
            cap: 20,
        },
    )
    .unwrap()
}

fn names(result: &remind_me_core::entity::EntityTraverseResult) -> Vec<String> {
    let mut out: Vec<String> = result.entities.iter().map(|e| e.name.clone()).collect();
    out.sort();
    out
}

/// A → B → C → D, so each extra hop reaches exactly one more entity.
fn chain(conn: &Connection) -> Vec<String> {
    let ids: Vec<String> = ["Ada", "Bailey", "Cleo", "Dana"]
        .iter()
        .map(|n| entity(conn, n, &[]))
        .collect();
    relate(conn, 1, &ids[0], "knows", &ids[1]);
    relate(conn, 2, &ids[1], "knows", &ids[2]);
    relate(conn, 3, &ids[2], "knows", &ids[3]);
    ids
}

#[test]
fn one_hop_returns_direct_relations_only() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    chain(&conn);

    let result = traverse(&conn, "Ada", 1);

    assert!(result.found);
    assert_eq!(result.hops, Some(1));
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].relation, "knows");
    assert_eq!(result.edges[0].hop, 1);
    assert_eq!(names(&result), vec!["Ada", "Bailey"]);
}

#[test]
fn two_and_three_hops_reach_further() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    chain(&conn);

    let two = traverse(&conn, "Ada", 2);
    assert_eq!(two.edges.len(), 2);
    assert_eq!(names(&two), vec!["Ada", "Bailey", "Cleo"]);
    assert_eq!(two.edges.iter().filter(|e| e.hop == 2).count(), 1);

    let three = traverse(&conn, "Ada", 3);
    assert_eq!(three.edges.len(), 3);
    assert_eq!(names(&three), vec!["Ada", "Bailey", "Cleo", "Dana"]);
}

#[test]
fn traversal_follows_edges_in_both_directions() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let ada = entity(&conn, "Ada", &[]);
    let bailey = entity(&conn, "Bailey", &[]);
    // Ada is the *object*; a subject-only walk would find nothing from her.
    relate(&conn, 1, &bailey, "introduced", &ada);

    let result = traverse(&conn, "Ada", 1);

    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].subject_name, "Bailey");
    assert_eq!(result.edges[0].object_name, "Ada");
}

#[test]
fn hops_are_clamped_rather_than_rejected() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    chain(&conn);

    // The schema bounds hops to 1..=3; a caller that ignores it gets a bounded
    // walk rather than an error.
    let high = traverse(&conn, "Ada", 99);
    assert_eq!(high.hops, Some(3));
    assert_eq!(high.edges.len(), 3);

    let low = traverse(&conn, "Ada", 0);
    assert_eq!(low.hops, Some(1));
    assert_eq!(low.edges.len(), 1);
}

#[test]
fn a_cycle_terminates() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = entity(&conn, "Ada", &[]);
    let b = entity(&conn, "Bailey", &[]);
    relate(&conn, 1, &a, "knows", &b);
    relate(&conn, 2, &b, "knows", &a);

    // Three hops around a two-node cycle. Both edges are found at hop 1 —
    // both endpoints are already in the frontier — leaving nothing new to
    // expand, so hops 2 and 3 have an empty frontier and stop.
    let result = traverse(&conn, "Ada", 3);

    assert_eq!(result.edges.len(), 2);
    assert!(result.edges.iter().all(|e| e.hop == 1));
    assert_eq!(names(&result), vec!["Ada", "Bailey"]);
}

#[test]
fn a_self_relation_terminates() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = entity(&conn, "Ada", &[]);
    relate(&conn, 1, &a, "knows", &a);

    let result = traverse(&conn, "Ada", 3);

    assert_eq!(result.edges.len(), 1, "the edge is returned exactly once");
    assert_eq!(names(&result), vec!["Ada"]);
}

#[test]
fn the_start_node_resolves_by_alias() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let bailey = entity(&conn, "Bailey Robertson", &["Bailey", "BR"]);
    let ada = entity(&conn, "Ada", &[]);
    relate(&conn, 1, &bailey, "knows", &ada);

    let result = traverse(&conn, "  br  ", 1);

    assert!(
        result.found,
        "an alias must resolve, casing and spacing aside"
    );
    assert_eq!(result.entity.as_ref().unwrap().name, "Bailey Robertson");
    assert_eq!(result.edges.len(), 1);
}

#[test]
fn a_canonical_name_beats_an_alias_on_another_entity() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // "Ada" is one entity's canonical name and another's alias. The canonical
    // name is the thing itself; the alias is a nickname for something else.
    entity(&conn, "Ada", &[]);
    entity(&conn, "Adelaide", &["Ada"]);

    let resolved = resolve_entity(&conn, "Ada").unwrap().unwrap();

    assert_eq!(resolved.name, "Ada");
    assert_eq!(resolved.id, entity_id("Ada"));
}

#[test]
fn an_unknown_entity_reports_not_found_rather_than_erroring() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity(&conn, "Ada", &[]);

    let result = traverse(&conn, "Nobody", 2);

    assert!(!result.found);
    assert_eq!(result.query.as_deref(), Some("Nobody"));
    assert!(result.message.unwrap().contains("Nobody"));
    assert!(result.entity.is_none());
    assert!(result.edges.is_empty());
}

#[test]
fn a_blank_name_resolves_to_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity(&conn, "Ada", &[]);

    assert!(resolve_entity(&conn, "   ").unwrap().is_none());
}

#[test]
fn an_isolated_entity_returns_itself_and_no_edges() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    entity(&conn, "Ada", &[]);

    let result = traverse(&conn, "Ada", 3);

    assert!(result.found);
    assert!(result.edges.is_empty());
    assert_eq!(names(&result), vec!["Ada"]);
}

#[test]
fn the_relation_filter_matches_exactly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = entity(&conn, "Ada", &[]);
    let b = entity(&conn, "Bailey", &[]);
    let c = entity(&conn, "Cleo", &[]);
    relate(&conn, 1, &a, "knows", &b);
    relate(&conn, 2, &a, "works_with", &c);

    let filtered = traverse_from_name(
        &conn,
        &EntityTraverseInput {
            name: "Ada".into(),
            hops: 1,
            relation: Some("knows".into()),
            cap: 20,
        },
    )
    .unwrap();

    assert_eq!(filtered.edges.len(), 1);
    assert_eq!(filtered.edges[0].object_name, "Bailey");

    let unmatched = traverse_from_name(
        &conn,
        &EntityTraverseInput {
            name: "Ada".into(),
            hops: 1,
            relation: Some("Knows".into()),
            cap: 20,
        },
    )
    .unwrap();
    assert!(
        unmatched.edges.is_empty(),
        "the filter is exact, not case-folded"
    );
}

#[test]
fn the_cap_bounds_the_edges_returned() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let hub = entity(&conn, "Ada", &[]);
    for i in 0..10 {
        let other = entity(&conn, &format!("Person {}", i), &[]);
        relate(&conn, i + 1, &hub, "knows", &other);
    }

    let capped = traverse_from_name(
        &conn,
        &EntityTraverseInput {
            name: "Ada".into(),
            hops: 3,
            relation: None,
            cap: 4,
        },
    )
    .unwrap();

    assert_eq!(capped.edges.len(), 4);
    // The seed plus one endpoint per returned edge — the cap must bound the
    // entity list too, not just the edges.
    assert_eq!(capped.entities.len(), 5);
}

#[test]
fn the_seed_is_first_and_entities_are_unique() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = entity(&conn, "Ada", &[]);
    let b = entity(&conn, "Bailey", &[]);
    // Two edges between the same pair: Bailey must still appear once.
    relate(&conn, 1, &a, "knows", &b);
    relate(&conn, 2, &a, "works_with", &b);

    let result = traverse(&conn, "Ada", 2);

    assert_eq!(result.edges.len(), 2);
    assert_eq!(result.entities.len(), 2);
    assert_eq!(result.entities[0].name, "Ada", "the seed comes first");
}
