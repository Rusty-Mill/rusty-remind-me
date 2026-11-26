//! Coverage for `metadata.occurred_at`, when a memory's event happened as
//! distinct from when it was recorded (card #62, pack D86 and D91): checked
//! on every write that supplies metadata, read by `time_field: occurred`.

use chrono::{Duration, Utc};
use remind_me_core::capture::{auto_capture, decompose};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::time_window::{RawTimeWindow, TimeField, TimeWindow};
use remind_me_core::{
    AtomicFact, AutoCaptureInput, Database, DecomposeInput, MemoryAddInput, MemorySearchInput,
    MemoryUpdateInput, UpdateOutcome,
};
use serde_json::{json, Value};

fn days_ago(days: i64) -> String {
    (Utc::now() - Duration::days(days)).to_rfc3339()
}

fn add_input(content: &str, metadata: Value) -> MemoryAddInput {
    MemoryAddInput {
        content: content.to_string(),
        category: "fact".to_string(),
        source: "manual".into(),
        metadata,
        ..Default::default()
    }
}

fn add(store: &Store<'_>, content: &str, metadata: Value) -> String {
    queries::add_memory(store, add_input(content, metadata))
        .unwrap()
        .id
}

fn update_metadata(store: &Store<'_>, id: &str, metadata: Option<Value>) -> UpdateOutcome {
    queries::update_memory(
        store,
        &MemoryUpdateInput {
            memory_id: id.to_string(),
            content: None,
            category: None,
            tags: Some(vec!["edited".into()]),
            metadata,
            sensitive: None,
            clear_superseded: false,
            project: None,
        },
    )
    .unwrap()
}

/// `query` over the window 40 to 20 days back on `field`.
fn search(store: &Store<'_>, query: &str, field: TimeField) -> Vec<String> {
    let raw = RawTimeWindow {
        since: Some("40d".into()),
        until: Some("20d".into()),
        time_field: Some(field),
    };
    let input = MemorySearchInput {
        query: query.to_string(),
        limit: 50,
        token_budget: 100_000,
        include_dormant: true,
        window: TimeWindow::resolve(&raw, Utc::now()).unwrap(),
        ..Default::default()
    };
    queries::search_memories(store, &input)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

#[test]
fn a_memory_written_today_about_last_month_is_found_by_when_it_happened() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let point = add(
        &store,
        "quokka release shipped",
        json!({ "occurred_at": days_ago(30) }),
    );
    let span = add(
        &store,
        "quokka migration ran",
        json!({ "occurred_at": { "from": days_ago(45), "to": days_ago(35) } }),
    );
    let plain = add(&store, "quokka note written today", json!({}));

    for query in ["", "quokka"] {
        let mut hits = search(&store, query, TimeField::Occurred);
        hits.sort();
        let mut want = vec![point.clone(), span.clone()];
        want.sort();
        assert_eq!(hits, want, "{query:?}");
        assert!(!hits.contains(&plain));
        // The record time is unchanged: nothing was written in that window.
        assert!(
            search(&store, query, TimeField::Created).is_empty(),
            "{query:?}"
        );
    }
}

#[test]
fn a_malformed_occurred_at_is_refused_on_add_naming_the_key() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for (value, key) in [
        (json!("last Friday"), "`occurred_at`"),
        (json!({ "from": days_ago(1) }), "`occurred_at.to`"),
        (
            json!({ "from": days_ago(1), "to": days_ago(2) }),
            "`occurred_at.to`",
        ),
    ] {
        let e = queries::add_memory(&store, add_input("x", json!({ "occurred_at": value })))
            .unwrap_err()
            .to_string();
        assert!(e.contains(key), "{value}: {e}");
    }
    assert!(queries::list_memories(&store, &Default::default())
        .unwrap()
        .memories
        .is_empty());
}

#[test]
fn update_can_set_and_clear_occurred_at() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka launch", json!({}));
    assert!(search(&store, "", TimeField::Occurred).is_empty());

    update_metadata(&store, &id, Some(json!({ "occurred_at": days_ago(30) })));
    assert_eq!(search(&store, "", TimeField::Occurred), vec![id.clone()]);

    update_metadata(&store, &id, Some(json!({})));
    assert!(search(&store, "", TimeField::Occurred).is_empty());

    let e = queries::update_memory(
        &store,
        &MemoryUpdateInput {
            memory_id: id.clone(),
            content: None,
            category: None,
            tags: None,
            metadata: Some(json!({ "occurred_at": "soon" })),
            sensitive: None,
            clear_superseded: false,
            project: None,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("`occurred_at`"), "{e}");
}

#[test]
fn a_malformed_stored_value_reads_as_absent_and_never_blocks_an_edit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    Memories::new(&store)
        .insert(&NewMemory {
            metadata: json!({ "occurred_at": "around then" }),
            ..NewMemory::new("old", "quokka from before the check", &days_ago(30))
        })
        .unwrap();

    // Read on created_at, so it sits in the window as it always has.
    assert_eq!(search(&store, "", TimeField::Occurred), ["old"]);
    // An edit that leaves metadata alone does not revalidate it.
    assert!(matches!(
        update_metadata(&store, "old", None),
        UpdateOutcome::Updated(_)
    ));
}

fn capture_input(metadata: Value) -> AutoCaptureInput {
    AutoCaptureInput {
        conversation: "user: when did we ship\nassistant: October 3rd".into(),
        summary: "We shipped the quokka release on 2026-10-03.".into(),
        title: String::new(),
        tags: vec![],
        category: "conversation".into(),
        metadata,
        project: None,
    }
}

#[test]
fn auto_capture_checks_occurred_at_before_writing_either_half() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let e = auto_capture(&store, &capture_input(json!({ "occurred_at": 20261003 })))
        .unwrap_err()
        .to_string();
    assert!(e.contains("`occurred_at`"), "{e}");
    assert!(queries::list_memories(&store, &Default::default())
        .unwrap()
        .memories
        .is_empty());

    let ok = auto_capture(
        &store,
        &capture_input(json!({ "occurred_at": "2026-10-03T12:00:00Z" })),
    )
    .unwrap();
    let summary = queries::get_memory_by_id(&store, &ok.summary_id)
        .unwrap()
        .unwrap();
    assert_eq!(summary.metadata["occurred_at"], "2026-10-03T12:00:00Z");
}

#[test]
fn decompose_checks_each_facts_occurred_at() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let captured = auto_capture(&store, &capture_input(json!({}))).unwrap();
    let fact = |metadata: Value| AtomicFact {
        content: "The quokka release shipped on 2026-10-03.".into(),
        memory_type: None,
        extra_tags: vec![],
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        metadata: Some(metadata),
    };
    let run = |facts: Vec<AtomicFact>| {
        decompose(
            &store,
            &DecomposeInput {
                capture_id: captured.capture_id.clone(),
                facts,
                capture_revision: None,
            },
        )
    };
    let e = run(vec![
        fact(json!({ "occurred_at": "2026-10-03T00:00:00Z" })),
        fact(json!({ "occurred_at": { "from": "x", "to": "2026-10-03T00:00:00Z" } })),
    ])
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("facts[1]") && e.contains("`occurred_at.from`"),
        "{e}"
    );

    let written = run(vec![fact(json!({ "occurred_at": "2026-10-03T00:00:00Z" }))])
        .unwrap()
        .unwrap();
    assert_eq!(written.fact_ids.len(), 1);
}
