//! Refinement due flag and run record against a real store (Mill Board
//! #1554, D104). The thresholds' logic is unit-tested in `refine.rs`; this
//! checks the store reads behind it and the record's write-time check.

use chrono::{Duration, Utc};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries::add_memory;
use remind_me_core::db::Store;
use remind_me_core::refine::{status, Due, RUN_CATEGORY};
use remind_me_core::{Database, MemoryAddInput};
use serde_json::{json, Value};

/// `count` captures last touched two days ago, so settled and idle.
fn settled_captures(store: &Store<'_>, count: usize) {
    let then = (Utc::now() - Duration::days(2)).to_rfc3339();
    for i in 0..count {
        Memories::new(store)
            .insert(&NewMemory {
                category: "dialog".to_string(),
                capture_id: Some(format!("cap_{i:03}")),
                ..NewMemory::new(format!("mem_cap_{i:03}"), "a session", &then)
            })
            .unwrap();
    }
}

fn run_record(metadata: Value) -> MemoryAddInput {
    MemoryAddInput {
        content: "Refinement run: 50 sessions".into(),
        category: RUN_CATEGORY.into(),
        metadata,
        ..MemoryAddInput::default()
    }
}

fn valid_run() -> Value {
    let now = Utc::now().to_rfc3339();
    json!({"run": {"decomposed": ["cap_000"], "created": [], "promoted": [],
                    "started_at": now, "finished_at": now}})
}

#[test]
fn an_empty_store_waits_on_the_backlog() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let s = status(&store, Utc::now());
    assert_eq!(
        s.due,
        Due::Backlog {
            settled_backlog: 0,
            needed: 50
        }
    );
    assert_eq!(s.last_run_id, None);
}

#[test]
fn a_settled_backlog_is_due_until_a_run_is_recorded() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    settled_captures(&store, 50);

    assert_eq!(
        status(&store, Utc::now()).due,
        Due::Due {
            settled_backlog: 50
        }
    );

    let record = add_memory(&store, run_record(valid_run())).unwrap();
    let after = status(&store, Utc::now());
    assert!(matches!(
        after.due,
        Due::Cooldown {
            settled_backlog: 50,
            ..
        }
    ));
    assert_eq!(after.last_run_id.as_deref(), Some(record.id.as_str()));
}

#[test]
fn a_run_record_without_its_ids_and_times_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let missing = add_memory(&store, run_record(json!({}))).unwrap_err();
    let bad_ids = add_memory(
        &store,
        run_record(json!({"run": {"created": "mem_x",
            "started_at": "2026-11-27T00:00:00Z", "finished_at": "2026-11-27T00:10:00Z"}})),
    )
    .unwrap_err();

    assert!(missing.to_string().contains("run"), "{missing}");
    assert!(bad_ids.to_string().contains("run.created"), "{bad_ids}");
}

#[test]
fn server_status_reports_the_refine_state() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let report = remind_me_core::status::server_status(&store).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["refine"]["state"], "backlog");
    assert_eq!(json["refine"]["needed"], 50);
}
