//! Coverage for `remind_me_vitality_report`.
//!
//! Age is simulated by backdating `accessed_at` directly, because nothing
//! in the crate updates that column after insert and there is no clock to move.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::vitality::{
    build_vitality_report, effective_vitality, is_dormant, VITALITY_FLOOR,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, category: &str) -> String {
    let input = MemoryAddInput {
        sensitive: false,
        content: content.to_string(),
        category: category.to_string(),
        tags: vec![],
        source: "manual".to_string(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
    };
    queries::add_memory(conn, input).expect("add failed").id
}

/// Backdate a memory's last access so elapsed-days decay has something to bite.
fn age_by_days(conn: &Connection, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    conn.execute(
        "UPDATE memories SET accessed_at = ?, created_at = ? WHERE id = ?",
        rusqlite::params![when, when, id],
    )
    .unwrap();
}

fn set_access_count(conn: &Connection, id: &str, count: i64) {
    conn.execute(
        "UPDATE memories SET access_count = ? WHERE id = ?",
        rusqlite::params![count, id],
    )
    .unwrap();
}

#[test]
fn empty_vault_reports_zeroes_without_dividing_by_zero() {
    let db = Database::open_in_memory().unwrap();
    let r = build_vitality_report(&db.conn()).unwrap();

    assert_eq!(r.total_memories, 0);
    assert_eq!(r.active_count, 0);
    assert_eq!(r.dormant_count, 0);
    assert_eq!(r.average_vitality, 0.0);
    assert_eq!(r.vault_health_score, "0%");
    assert!(r.decay_distribution.is_empty());
}

#[test]
fn every_bucket_label_is_present_even_when_empty() {
    let db = Database::open_in_memory().unwrap();
    let r = build_vitality_report(&db.conn()).unwrap();

    let labels: Vec<&String> = r.vitality_buckets.keys().collect();
    assert_eq!(
        labels,
        vec!["0.00-0.05", "0.05-0.25", "0.25-0.50", "0.50-0.75", "0.75+"]
    );
}

#[test]
fn buckets_always_sum_to_the_total() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let fresh = add(&conn, "fresh", "fact");
    let middling = add(&conn, "middling", "action_item");
    let ancient = add(&conn, "ancient", "action_item");

    age_by_days(&conn, &middling, 8);
    age_by_days(&conn, &ancient, 365);
    // An accessed memory scores above 1.0 and must land in the open top bucket.
    set_access_count(&conn, &fresh, 1);

    let r = build_vitality_report(&conn).unwrap();
    let summed: usize = r.vitality_buckets.values().sum();
    assert_eq!(
        summed, r.total_memories,
        "DI-04: a closed top bucket would drop rows and break this sum"
    );
    assert_eq!(r.total_memories, 3);
}

#[test]
fn decay_is_applied_at_report_time_not_read_from_the_stored_column() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "ages badly", "action_item"); // decay 0.20

    let stored_before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    age_by_days(&conn, &id, 365);
    let stored_after = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    assert_eq!(
        stored_before.vitality, stored_after.vitality,
        "the stored column is a write-time snapshot and does not move"
    );

    let effective = effective_vitality(&stored_after, Utc::now());
    assert!(
        effective < stored_after.vitality,
        "effective {} should be below stored {}",
        effective,
        stored_after.vitality
    );
    assert!(
        is_dormant(effective),
        "a year at decay 0.20 should be well under the floor, got {}",
        effective
    );
}

#[test]
fn dormancy_counts_come_from_effective_vitality() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "still fresh", "fact");
    let stale = add(&conn, "long forgotten", "action_item");
    age_by_days(&conn, &stale, 365);

    let r = build_vitality_report(&conn).unwrap();
    assert_eq!(r.total_memories, 2);
    assert_eq!(r.dormant_count, 1, "the aged memory must count as dormant");
    assert_eq!(r.active_count, 1);
    assert_eq!(r.vault_health_score, "50%");
}

#[test]
fn a_fresh_vault_is_fully_healthy() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..4 {
        add(&conn, &format!("memory {}", i), "fact");
    }

    let r = build_vitality_report(&conn).unwrap();
    assert_eq!(r.dormant_count, 0);
    assert_eq!(r.vault_health_score, "100%");
}

#[test]
fn bridge_protection_halves_decay_for_heavily_accessed_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let plain = add(&conn, "rarely used", "action_item");
    let bridge = add(&conn, "heavily used", "action_item");

    age_by_days(&conn, &plain, 30);
    age_by_days(&conn, &bridge, 30);
    // BRIDGE_THRESHOLD is 10 accesses; at or above it decay is halved.
    set_access_count(&conn, &bridge, 10);

    let now = Utc::now();
    let plain_v = effective_vitality(
        &queries::get_memory_by_id(&conn, &plain).unwrap().unwrap(),
        now,
    );
    let bridge_v = effective_vitality(
        &queries::get_memory_by_id(&conn, &bridge).unwrap().unwrap(),
        now,
    );

    assert!(
        bridge_v > plain_v,
        "bridge-protected {} should outlive unprotected {}",
        bridge_v,
        plain_v
    );
}

#[test]
fn decay_distribution_groups_by_category() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a", "fact");
    add(&conn, "b", "fact");
    add(&conn, "c", "decision");

    let r = build_vitality_report(&conn).unwrap();
    assert_eq!(r.decay_distribution.get("fact"), Some(&2));
    assert_eq!(r.decay_distribution.get("decision"), Some(&1));
}

#[test]
fn deleted_memories_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let doomed = add(&conn, "going", "fact");
    add(&conn, "staying", "fact");
    queries::delete_memory(&conn, &doomed).unwrap();

    let r = build_vitality_report(&conn).unwrap();
    assert_eq!(r.total_memories, 1);
    assert_eq!(r.decay_distribution.get("fact"), Some(&1));
}

#[test]
fn floor_boundary_is_exclusive_below() {
    // is_dormant is `< VITALITY_FLOOR`, so a value exactly at the floor is
    // still active. Pinning this because an off-by-one here silently changes
    // what search returns by default.
    assert!(!is_dormant(VITALITY_FLOOR));
    assert!(is_dormant(VITALITY_FLOOR - 1e-9));
}

#[test]
fn report_serializes_with_the_reference_field_names() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a", "fact");

    let value = serde_json::to_value(build_vitality_report(&conn).unwrap()).unwrap();
    for field in [
        "total_memories",
        "active_count",
        "dormant_count",
        "average_vitality",
        "vault_health_score",
        "decay_distribution",
        "vitality_buckets",
    ] {
        assert!(value.get(field).is_some(), "missing field {}", field);
    }
    assert!(
        value["vault_health_score"].is_string(),
        "health is a string"
    );
}
