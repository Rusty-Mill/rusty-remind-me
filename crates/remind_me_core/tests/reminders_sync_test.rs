//! `remind_at` across sync (gap T1a, issue #116).
//!
//! Its own test binary for the same reason as `sensitive_sync_test.rs`: the
//! sync switch is three process-wide env vars, and `reminders_test.rs` asserts
//! single-node behaviour throughout.
//!
//! Both halves have to be asserted, and asserting them separately is what let
//! the `sensitive` bug through once already — the payload carried the field
//! and the receiving side silently dropped it, so each side's test passed
//! while the join did nothing.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::models::ReminderWindow;
use remind_me_core::reminders::{list_reminders, set_reminder};
use remind_me_core::sync::{upsert_record, SyncRecord, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

fn enable_sync() {
    crate::test_env::set_var(NODE_ID_ENV, "node-reminders-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
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
}

fn future(hours: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339()
}

#[test]
fn the_outbox_payload_carries_the_reminder() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "renew the registration");
    conn.execute("DELETE FROM sync_outbox", []).unwrap();

    let when = future(30);
    set_reminder(&conn, &id, Some(&when)).unwrap();

    let payload: Option<String> = conn
        .query_row(
            "SELECT json_extract(payload, '$.remind_at') FROM sync_outbox
              WHERE memory_id = ? ORDER BY id DESC LIMIT 1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();

    // Setting a reminder has to produce an outbox row at all: the trigger only
    // fires when `updated_at` actually moves, which is why `set_reminder`
    // bumps it rather than writing `remind_at` alone.
    assert!(
        payload.is_some(),
        "a peer rebuilds the memory from this payload alone"
    );
}

#[test]
fn an_incoming_reminder_is_applied_and_shows_up_in_the_window() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let when = future(12);
    let record = SyncRecord {
        id: "mem_from_laptop".into(),
        content: "pick up the prescription".into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        created_at: "2030-01-01T00:00:00+00:00".into(),
        updated_at: "2030-01-01T00:00:00+00:00".into(),
        capture_id: None,
        node_id: Some("laptop".into()),
        client: "test".into(),
        accessed_at: None,
        access_count: 0,
        decay_rate: 0.1,
        vitality: 1.0,
        base_weight: 1.0,
        status: "active".into(),
        memory_type: "unclassified".into(),
        source_capture_id: None,
        subject: None,
        predicate: None,
        object: None,
        superseded_by: None,
        deleted_at: None,
        sensitive: false,
        remind_at: Some(when.clone()),
    };

    upsert_record(&conn, &record).unwrap();

    // Dropped on receipt, a reminder set on your laptop would be invisible on
    // your desktop while every other property of the same memory arrived
    // intact — the kind of half-wiring nobody finds until they miss something.
    let upcoming = list_reminders(&conn, ReminderWindow::Upcoming, 20).unwrap();
    assert_eq!(upcoming.len(), 1);
    assert_eq!(upcoming[0].id, "mem_from_laptop");
    assert_eq!(upcoming[0].remind_at.as_deref(), Some(when.as_str()));
}

#[test]
fn a_record_with_no_remind_at_key_still_applies() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // A node predating the v27 schema sends no `remind_at` key at all. It has
    // to read as "no reminder" rather than failing the whole pull — the
    // failure mode `sensitive` hit, where one unreadable field silently
    // dropped every memory record in the batch.
    let raw = serde_json::json!({
        "id": "mem_old_node",
        "content": "from an older node",
        "created_at": "2030-01-01T00:00:00+00:00",
        "updated_at": "2030-01-01T00:00:00+00:00",
    });
    let record: SyncRecord = serde_json::from_value(raw).expect("an older record still parses");

    assert!(record.remind_at.is_none());
    upsert_record(&conn, &record).unwrap();
    assert!(list_reminders(&conn, ReminderWindow::All, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn a_cleared_reminder_propagates_as_a_clear_rather_than_being_ignored() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "cancelled appointment");
    set_reminder(&conn, &id, Some(&future(20))).unwrap();
    assert_eq!(
        list_reminders(&conn, ReminderWindow::All, 20)
            .unwrap()
            .len(),
        1
    );

    // The other node cleared it. A NULL that only means "no opinion" would
    // leave the reminder standing here forever, and cancelling would be the
    // one edit that could never reach a second machine.
    let record = SyncRecord {
        id: id.clone(),
        content: "cancelled appointment".into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        created_at: "2030-01-01T00:00:00+00:00".into(),
        // Must win LWW, or nothing but tags/metadata is applied.
        updated_at: "2099-01-01T00:00:00+00:00".into(),
        capture_id: None,
        node_id: Some("laptop".into()),
        client: "test".into(),
        accessed_at: None,
        access_count: 0,
        decay_rate: 0.1,
        vitality: 1.0,
        base_weight: 1.0,
        status: "active".into(),
        memory_type: "unclassified".into(),
        source_capture_id: None,
        subject: None,
        predicate: None,
        object: None,
        superseded_by: None,
        deleted_at: None,
        sensitive: false,
        remind_at: None,
    };

    upsert_record(&conn, &record).unwrap();

    assert!(list_reminders(&conn, ReminderWindow::All, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn a_losing_record_does_not_clear_a_locally_set_reminder() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "still scheduled");
    set_reminder(&conn, &id, Some(&future(20))).unwrap();

    let record = SyncRecord {
        id: id.clone(),
        content: "still scheduled".into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        created_at: "2000-01-01T00:00:00+00:00".into(),
        // Older than the local row, so it loses LWW.
        updated_at: "2000-01-01T00:00:00+00:00".into(),
        capture_id: None,
        node_id: Some("laptop".into()),
        client: "test".into(),
        accessed_at: None,
        access_count: 0,
        decay_rate: 0.1,
        vitality: 1.0,
        base_weight: 1.0,
        status: "active".into(),
        memory_type: "unclassified".into(),
        source_capture_id: None,
        subject: None,
        predicate: None,
        object: None,
        superseded_by: None,
        deleted_at: None,
        sensitive: false,
        remind_at: None,
    };

    upsert_record(&conn, &record).unwrap();

    // LWW protects the reminder exactly as it protects content: a stale copy
    // of the memory arriving from a node that never knew about the reminder
    // must not silently cancel it.
    assert_eq!(
        list_reminders(&conn, ReminderWindow::All, 20)
            .unwrap()
            .len(),
        1
    );
}
