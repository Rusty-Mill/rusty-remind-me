//! Coverage for `remind_me_vitality_report`.
//!
//! Age is simulated by backdating `accessed_at` directly, because nothing
//! in the crate updates that column after insert and there is no clock to move.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::vitality::{
    build_vitality_report, effective_vitality, is_dormant, VITALITY_FLOOR,
};
use remind_me_core::{Database, MemoryAddInput};

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
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
    queries::add_memory(store, input).expect("add failed").id
}

/// Backdate a memory's last access so elapsed-days decay has something to bite.
fn age_by_days(store: &Store<'_>, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    testing::set_memory_column(store, id, "accessed_at", when.as_str()).unwrap();
    testing::set_memory_column(store, id, "created_at", when).unwrap();
}

fn set_access_count(store: &Store<'_>, id: &str, count: i64) {
    testing::set_memory_column(store, id, "access_count", count).unwrap();
}

#[test]
fn empty_vault_reports_zeroes_without_dividing_by_zero() {
    let db = Database::open_in_memory().unwrap();
    let r = build_vitality_report(&db.store()).unwrap();

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
    let r = build_vitality_report(&db.store()).unwrap();

    let labels: Vec<&String> = r.vitality_buckets.keys().collect();
    assert_eq!(
        labels,
        vec!["0.00-0.05", "0.05-0.25", "0.25-0.50", "0.50-0.75", "0.75+"]
    );
}

#[test]
fn buckets_always_sum_to_the_total() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fresh = add(&store, "fresh", "fact");
    let middling = add(&store, "middling", "action_item");
    let ancient = add(&store, "ancient", "action_item");

    age_by_days(&store, &middling, 8);
    age_by_days(&store, &ancient, 365);
    // An accessed memory scores above 1.0 and must land in the open top bucket.
    set_access_count(&store, &fresh, 1);

    let r = build_vitality_report(&store).unwrap();
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
    let store = db.store();
    let id = add(&store, "ages badly", "action_item"); // decay 0.20

    let stored_before = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    age_by_days(&store, &id, 365);
    let stored_after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();

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
    let store = db.store();
    add(&store, "still fresh", "fact");
    let stale = add(&store, "long forgotten", "action_item");
    age_by_days(&store, &stale, 365);

    let r = build_vitality_report(&store).unwrap();
    assert_eq!(r.total_memories, 2);
    assert_eq!(r.dormant_count, 1, "the aged memory must count as dormant");
    assert_eq!(r.active_count, 1);
    assert_eq!(r.vault_health_score, "50%");
}

#[test]
fn a_fresh_vault_is_fully_healthy() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..4 {
        add(&store, &format!("memory {}", i), "fact");
    }

    let r = build_vitality_report(&store).unwrap();
    assert_eq!(r.dormant_count, 0);
    assert_eq!(r.vault_health_score, "100%");
}

#[test]
fn bridge_protection_halves_decay_for_heavily_accessed_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let plain = add(&store, "rarely used", "action_item");
    let bridge = add(&store, "heavily used", "action_item");

    age_by_days(&store, &plain, 30);
    age_by_days(&store, &bridge, 30);
    // BRIDGE_THRESHOLD is 10 accesses; at or above it decay is halved.
    set_access_count(&store, &bridge, 10);

    let now = Utc::now();
    let plain_v = effective_vitality(
        &queries::get_memory_by_id(&store, &plain).unwrap().unwrap(),
        now,
    );
    let bridge_v = effective_vitality(
        &queries::get_memory_by_id(&store, &bridge).unwrap().unwrap(),
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
    let store = db.store();
    add(&store, "a", "fact");
    add(&store, "b", "fact");
    add(&store, "c", "decision");

    let r = build_vitality_report(&store).unwrap();
    assert_eq!(r.decay_distribution.get("fact"), Some(&2));
    assert_eq!(r.decay_distribution.get("decision"), Some(&1));
}

#[test]
fn deleted_memories_are_excluded() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let doomed = add(&store, "going", "fact");
    add(&store, "staying", "fact");
    queries::delete_memory(&store, &doomed).unwrap();

    let r = build_vitality_report(&store).unwrap();
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
    let store = db.store();
    add(&store, "a", "fact");

    let value = serde_json::to_value(build_vitality_report(&store).unwrap()).unwrap();
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
