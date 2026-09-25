//! Response-envelope parity for the tools in #201.
//!
//! Each key here was found missing by invoking the tool on both
//! implementations against a freshly seeded database and diffing the response
//! keys — response *fields* had never been compared before, only tool names
//! and route lists, which is how these survived.
//!
//! Asserted on the **serialised** value throughout. A struct field that never
//! reached the JSON would satisfy a test written against the struct and change
//! nothing a client can see, which is the same shape as the bug.

use remind_me_core::db::queries;
use remind_me_core::models::{
    AnnotateInput, ExportFormat, MemoryAddInput, MemoryAnnotation, MemoryListInput, ReconcileReport,
};
use remind_me_core::{watcher, Database};
use rusqlite::Connection;
use serde_json::Value;

fn seed(conn: &Connection, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            queries::add_memory(
                conn,
                MemoryAddInput {
                    content: format!("seed {i}"),
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
        })
        .collect()
}

#[test]
fn list_reports_count_alongside_total() {
    // `count` is this page; `total` is how many exist behind it. The reference
    // emits both, and a client written against it reads `count`.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    seed(&conn, 5);

    let input = MemoryListInput {
        limit: 2,
        ..Default::default()
    };
    let page = queries::list_memories(&conn, &input).unwrap();
    let json: Value = serde_json::to_value(&page).unwrap();

    assert_eq!(json["count"], 2, "count describes this page");
    assert_eq!(json["total"], 5, "total describes the whole set");
    assert_eq!(
        json["count"].as_u64().unwrap() as usize,
        page.memories.len(),
        "count must match the list it describes, not be a second opinion"
    );
}

#[test]
fn list_count_and_total_agree_when_a_page_holds_everything() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    seed(&conn, 3);

    // Explicit limit: `MemoryListInput::default()` derives `limit: 0`, which
    // `list_memories` clamps to LIST_LIMIT_MIN, while serde's default for an
    // omitted `limit` is 20. Same struct, two different defaults — not this
    // test's subject, so it does not rely on either.
    let input = MemoryListInput {
        limit: 10,
        ..Default::default()
    };
    let page = queries::list_memories(&conn, &input).unwrap();
    let json: Value = serde_json::to_value(&page).unwrap();

    assert_eq!(json["count"], 3);
    assert_eq!(json["total"], 3);
}

#[test]
fn annotate_reports_how_many_it_applied() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let ids = seed(&conn, 2);

    let input = AnnotateInput {
        annotations: ids
            .iter()
            .map(|id| MemoryAnnotation {
                memory_id: id.clone(),
                subject: Some("s".into()),
                predicate: Some("p".into()),
                object: Some("o".into()),
                entities: vec![],
            })
            .collect(),
    };
    let result = queries::annotate_memories(&conn, &input).unwrap();
    let json: Value = serde_json::to_value(&result).unwrap();

    assert_eq!(json["annotated"], 2);
    assert_eq!(
        json["annotated"].as_u64().unwrap() as usize,
        result.results.len(),
        "annotated must track the applied list"
    );
}

#[test]
fn annotate_reports_zero_when_nothing_applied() {
    // The value has to move with reality. A hardcoded count would pass the
    // test above and fail here.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let input = AnnotateInput {
        annotations: vec![MemoryAnnotation {
            memory_id: "mem_does_not_exist".into(),
            subject: Some("s".into()),
            predicate: Some("p".into()),
            object: Some("o".into()),
            entities: vec![],
        }],
    };
    let result = queries::annotate_memories(&conn, &input).unwrap();
    let json: Value = serde_json::to_value(&result).unwrap();

    assert_eq!(json["annotated"], 0);
    assert!(!result.errors.is_empty(), "the failure is still reported");
}

#[test]
fn export_reports_a_status() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    seed(&conn, 2);

    let input = remind_me_core::models::ExportInput {
        format: ExportFormat::Json,
        ..Default::default()
    };
    let result = remind_me_core::export::export_memories(&conn, &input).unwrap();
    let json: Value = serde_json::to_value(&result).unwrap();

    assert_eq!(json["status"], "ok");
    assert_eq!(json["exported"], 2);
}

#[test]
fn an_unreachable_reconcile_reports_hint_not_reason() {
    // A rename on the wire only. The reference calls this `hint`, and a client
    // keying on it found nothing here.
    let report = ReconcileReport::Unavailable {
        reason: "sync is not configured on this node".to_string(),
    };
    let json: Value = serde_json::to_value(&report).unwrap();

    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["hint"], "sync is not configured on this node");
    assert!(
        json.get("reason").is_none(),
        "the old key must be gone, not emitted alongside — two names for one \
         value is worse than either"
    );
}

#[test]
fn watch_status_distinguishes_configured_from_running() {
    // The point of `running`. `enabled` says directories are configured;
    // nothing said whether anything was actually scanning them until #203
    // added the loop, and `disabled_status()` describes a watcher that is
    // neither.
    let status = watcher::disabled_status();
    let json: Value = serde_json::to_value(&status).unwrap();

    assert_eq!(json["enabled"], false);
    assert_eq!(
        json["running"], false,
        "running must be reported, not inferred from enabled"
    );
    assert!(
        json.get("pending_wiki_compile").is_some(),
        "pending_wiki_compile must be present"
    );
}

#[test]
fn a_watcher_that_is_configured_but_not_looping_reports_running_false() {
    // Updated when the driver landed (#203), as its predecessor said it should
    // be. It no longer pins "the watcher never runs" — it pins the narrower
    // and still-true claim that `enabled` and `running` are independent: a
    // `WatchStatus` describing a configured watcher with no loop behind it
    // reports `running: false`. `watcher_driver_test.rs` covers the other
    // half, where a real loop reports `true`.
    let status = watcher::WatchStatus {
        enabled: true,
        running: false,
        pending_wiki_compile: 0,
        watch_dirs: vec!["/tmp/x".into()],
        rejected_dirs: vec![],
        interval_seconds: 60,
        grace_seconds: 5,
        scans: 0,
        last_scan_at: None,
        files_ingested: 0,
        files_skipped: 0,
        memories_superseded: 0,
        recent_errors: vec![],
        hint: None,
    };
    let json: Value = serde_json::to_value(&status).unwrap();

    assert_eq!(json["enabled"], true);
    assert_eq!(json["running"], false);
}
