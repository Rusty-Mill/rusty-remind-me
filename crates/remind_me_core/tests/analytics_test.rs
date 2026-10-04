//! Coverage for daily analytics snapshots (gap A1, issue #112).

use remind_me_core::analytics::{capture_snapshot, trend};
use remind_me_core::db::queries;
use remind_me_core::db::stats::StoreStats;
use remind_me_core::db::Store;
use remind_me_core::{AnalyticsSnapshot, CapturedSnapshot, Database, MemoryAddInput};

fn add(store: &Store<'_>, content: &str, category: &str) {
    queries::add_memory(
        store,
        MemoryAddInput {
            extract: true,
            attachments: vec![],
            content: content.to_string(),
            category: category.to_string(),
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
    .unwrap();
}

#[test]
fn a_snapshot_records_the_vaults_current_shape() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "one", "general");
    add(&store, "two", "engineering");
    add(&store, "three", "engineering");

    assert!(matches!(
        capture_snapshot(&store).unwrap(),
        CapturedSnapshot::Captured { .. }
    ));

    let series = trend(&store).unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].total_memories, 3);
    assert_eq!(series[0].category_counts.get("engineering"), Some(&2));
    assert_eq!(series[0].category_counts.get("general"), Some(&1));
    assert!(!series[0].vitality_buckets.is_empty());
}

#[test]
fn a_second_capture_on_the_same_day_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "one", "general");

    let first = capture_snapshot(&store).unwrap();
    add(&store, "two", "general");
    let second = capture_snapshot(&store).unwrap();

    // Idempotent per calendar *day*, not per timestamp. A server restarted
    // three times in a day would otherwise show three data points and the
    // trend would read as a spike that never happened.
    let CapturedSnapshot::Captured { id: first_id } = first else {
        panic!("first capture should have inserted");
    };
    assert_eq!(second, CapturedSnapshot::AlreadyToday { id: first_id });
    assert_eq!(trend(&store).unwrap().len(), 1);
    assert_eq!(
        trend(&store).unwrap()[0].total_memories,
        1,
        "the existing row must not be rewritten either"
    );
}

#[test]
fn the_series_is_oldest_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Plant history directly: the capture path is deliberately once-per-day,
    // so multi-day series cannot be produced by calling it in a loop.
    let stats = StoreStats::new(&store);
    for (day, total) in [("2026-01-01", 5), ("2026-01-03", 9), ("2026-01-02", 7)] {
        stats
            .insert_snapshot(&AnalyticsSnapshot {
                captured_at: format!("{day}T00:00:00+00:00"),
                total_memories: total,
                vitality_buckets: Default::default(),
                category_counts: Default::default(),
            })
            .unwrap();
    }

    let series = trend(&store).unwrap();

    // Oldest first, because the only consumer is a chart — a series that has
    // to be reversed before plotting is a trap the first caller falls into.
    assert_eq!(
        series.iter().map(|s| s.total_memories).collect::<Vec<_>>(),
        vec![5, 7, 9]
    );
}

#[test]
fn a_new_install_has_an_empty_series_not_an_error() {
    let db = Database::open_in_memory().unwrap();

    // Empty is meaningfully different from flat: no history yet, rather than
    // history showing no change.
    assert!(trend(&db.store()).unwrap().is_empty());
}
