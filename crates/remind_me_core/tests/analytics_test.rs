//! Coverage for daily analytics snapshots (gap A1, issue #112).

use remind_me_core::analytics::{capture_snapshot, trend};
use remind_me_core::db::queries;
use remind_me_core::{CapturedSnapshot, Database, MemoryAddInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, category: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
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
    let conn = db.conn();
    add(&conn, "one", "general");
    add(&conn, "two", "engineering");
    add(&conn, "three", "engineering");

    assert!(matches!(
        capture_snapshot(&conn).unwrap(),
        CapturedSnapshot::Captured { .. }
    ));

    let series = trend(&conn).unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].total_memories, 3);
    assert_eq!(series[0].category_counts.get("engineering"), Some(&2));
    assert_eq!(series[0].category_counts.get("general"), Some(&1));
    assert!(!series[0].vitality_buckets.is_empty());
}

#[test]
fn a_second_capture_on_the_same_day_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "one", "general");

    let first = capture_snapshot(&conn).unwrap();
    add(&conn, "two", "general");
    let second = capture_snapshot(&conn).unwrap();

    // Idempotent per calendar *day*, not per timestamp. A server restarted
    // three times in a day would otherwise show three data points and the
    // trend would read as a spike that never happened.
    let CapturedSnapshot::Captured { id: first_id } = first else {
        panic!("first capture should have inserted");
    };
    assert_eq!(second, CapturedSnapshot::AlreadyToday { id: first_id });
    assert_eq!(trend(&conn).unwrap().len(), 1);
    assert_eq!(
        trend(&conn).unwrap()[0].total_memories,
        1,
        "the existing row must not be rewritten either"
    );
}

#[test]
fn the_series_is_oldest_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Plant history directly: the capture path is deliberately once-per-day,
    // so multi-day series cannot be produced by calling it in a loop.
    for (day, total) in [("2026-01-01", 5), ("2026-01-03", 9), ("2026-01-02", 7)] {
        conn.execute(
            "INSERT INTO analytics_snapshots
                 (captured_at, total_memories, vitality_buckets, category_counts)
             VALUES (?, ?, '{}', '{}')",
            rusqlite::params![format!("{}T00:00:00+00:00", day), total],
        )
        .unwrap();
    }

    let series = trend(&conn).unwrap();

    // Oldest first, because the only consumer is a chart — a series that has
    // to be reversed before plotting is a trap the first caller falls into.
    assert_eq!(
        series.iter().map(|s| s.total_memories).collect::<Vec<_>>(),
        vec![5, 7, 9]
    );
}

#[test]
fn a_malformed_stored_value_does_not_take_the_chart_down() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO analytics_snapshots
             (captured_at, total_memories, vitality_buckets, category_counts)
         VALUES ('2026-01-01T00:00:00+00:00', 5, 'not json', '{}')",
        [],
    )
    .unwrap();

    let series = trend(&conn).unwrap();

    // One bad row degrades to empty maps rather than failing the whole read.
    // The alternative is a chart that goes blank because of a single row
    // nobody can see.
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].total_memories, 5);
    assert!(series[0].vitality_buckets.is_empty());
}

#[test]
fn a_new_install_has_an_empty_series_not_an_error() {
    let db = Database::open_in_memory().unwrap();

    // Empty is meaningfully different from flat: no history yet, rather than
    // history showing no change.
    assert!(trend(&db.conn()).unwrap().is_empty());
}
