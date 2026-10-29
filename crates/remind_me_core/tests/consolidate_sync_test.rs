//! A consolidation merge, seen from sync: the canonical's queued outbox
//! payload must carry the final evidence count and boosted `base_weight`, so a
//! peer applying it ends up with the same row as the node that merged.
//!
//! Sync is switched on process-wide for this whole binary, as `outbox_test.rs`
//! does, which is why this lives apart from `consolidate_test.rs`.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::consolidation::consolidate;
use remind_me_core::db::queries;
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::Store;
use remind_me_core::sync::{upsert_record, SyncRecord, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::testing;
use remind_me_core::{ConsolidateInput, Database, MemoryAddInput};
use serde_json::Value;
use std::collections::HashMap;

fn ensure_sync_enabled() {
    crate::test_env::set_var(NODE_ID_ENV, "node-consolidate-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn add_with_vector(store: &Store<'_>, content: &str, vector: &[f32]) -> String {
    let id = queries::add_memory(
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
    .id;
    let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
    Vectors::new(store).put(&id, 0, &bytes).unwrap();
    id
}

/// The latest queued payload for `id`, as the record a peer would apply: the
/// outbox keeps `tags`/`metadata` as JSON strings, decoded the way a push
/// decodes them.
fn last_queued(store: &Store<'_>, id: &str) -> SyncRecord {
    let row = testing::outbox_rows(store)
        .unwrap()
        .into_iter()
        .rev()
        .find(|r| r.memory_id == id)
        .expect("the canonical is queued");
    let mut payload: Value = serde_json::from_str(&row.payload).unwrap();
    for key in ["tags", "metadata"] {
        let raw = payload[key].as_str().unwrap().to_owned();
        payload[key] = serde_json::from_str(&raw).unwrap();
    }
    serde_json::from_value(payload).unwrap()
}

#[test]
fn the_queued_merge_carries_the_final_evidence_and_weight() {
    ensure_sync_enabled();
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let canonical = add_with_vector(&store, "the meeting moved to 3pm", &[1.0, 0.0]);
    let member = add_with_vector(&store, "meeting is now at 3pm", &[0.99, 0.14107]);
    testing::set_memory_column(&store, &canonical, "vitality", 0.9).unwrap();
    testing::set_memory_column(&store, &canonical, "access_count", 5).unwrap();
    testing::set_memory_column(&store, &member, "vitality", 0.5).unwrap();

    let summaries = HashMap::from([(canonical.clone(), "the meeting is at 3pm".to_string())]);
    let report = consolidate(
        &store,
        &ConsolidateInput {
            similarity_threshold: 0.85,
            dry_run: false,
            category: None,
            limit: 500,
            summaries: Some(summaries),
        },
    )
    .unwrap();
    assert_eq!(report["canonical_ids"][0], canonical, "{report}");
    assert_ne!(canonical, member);

    let stored = queries::get_memory_by_id(&store, &canonical)
        .unwrap()
        .unwrap();
    assert_eq!(stored.metadata["evidence"], 2);
    let queued = last_queued(&store, &canonical);
    assert_eq!(queued.metadata["evidence"], 2);
    assert_eq!(queued.base_weight, stored.base_weight);
    assert_eq!(queued.vitality, stored.vitality);

    let peer_db = Database::open_in_memory().unwrap();
    let peer = peer_db.store();
    upsert_record(&peer, &queued).unwrap();
    let replica = queries::get_memory_by_id(&peer, &canonical)
        .unwrap()
        .unwrap();
    assert_eq!(replica.metadata["evidence"], 2);
    assert_eq!(replica.base_weight, stored.base_weight);
    assert_eq!(replica.content, stored.content);
}
