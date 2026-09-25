//! Coverage for `remind_me_digest` (gap T5, issue #111).

use remind_me_core::db::queries;
use remind_me_core::digest::{build_digest, render_markdown, MAX_RECENT_MEMORIES};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, sensitive: bool) -> String {
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
            sensitive,
        },
    )
    .unwrap()
    .id
}

fn backdate(conn: &Connection, id: &str, days: i64) {
    let when = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    conn.execute(
        "UPDATE memories SET created_at = ? WHERE id = ?",
        rusqlite::params![when, id],
    )
    .unwrap();
}

#[test]
fn the_digest_lists_memories_from_the_window() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "this week", false);
    let old = add(&conn, "last month", false);
    backdate(&conn, &old, 30);

    let data = build_digest(&conn, 7).unwrap();

    assert_eq!(data.recent_memories.len(), 1);
    assert_eq!(data.recent_memories[0].content, "this week");
    assert_eq!(data.recent_total, 1);
}

#[test]
fn sensitive_memories_never_appear_and_there_is_no_override() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "ordinary", false);
    add(&conn, "private", true);

    let data = build_digest(&conn, 7).unwrap();

    // Unlike search and list, a digest has no `include_sensitive`. It is the
    // ambient, often-scheduled surface the flag exists to protect, with no
    // per-call caller intent to opt back in against. Counted as well as
    // listed — a total that included it would leak that something is there.
    assert_eq!(data.recent_memories.len(), 1);
    assert_eq!(data.recent_memories[0].content, "ordinary");
    assert_eq!(data.recent_total, 1);
    assert!(!render_markdown(&data).contains("private"));
}

#[test]
fn the_cap_is_visible_rather_than_silent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..(MAX_RECENT_MEMORIES + 5) {
        add(&conn, &format!("memory {}", i), false);
    }

    let data = build_digest(&conn, 7).unwrap();

    // The true count is carried separately, so a busy week reads as "20 of 25"
    // rather than silently looking like a quiet one.
    assert_eq!(data.recent_memories.len(), MAX_RECENT_MEMORIES);
    assert_eq!(data.recent_total, (MAX_RECENT_MEMORIES + 5) as i64);
    assert!(render_markdown(&data).contains(&format!("of {}", MAX_RECENT_MEMORIES + 5)));
}

#[test]
fn an_empty_window_says_so_rather_than_rendering_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let old = add(&conn, "ancient", false);
    backdate(&conn, &old, 400);

    let data = build_digest(&conn, 7).unwrap();
    let markdown = render_markdown(&data);

    // "Nothing new this week" is information; a blank section reads as a bug.
    assert!(data.recent_memories.is_empty());
    assert!(markdown.contains("Nothing new"));
}

#[test]
fn the_reminder_and_sync_sections_report_emptiness_rather_than_vanishing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "something", false);

    let data = build_digest(&conn, 7).unwrap();
    let markdown = render_markdown(&data);

    // Both sections were omitted while their subsystems did not exist, because
    // "Reminders: none" would have read as "you have nothing due" when the
    // truth was "nothing here can tell". Both now exist (#116, #114), so an
    // empty vault genuinely has nothing due and the section says so.
    assert!(data.reminders_upcoming.is_empty());
    assert!(data.reminders_overdue.is_empty());
    assert!(markdown.contains("## Reminders"));
    assert!(markdown.contains("_No reminders set._"));
    assert!(markdown.contains("## Sync health"));

    // Sync is off in the test environment, and saying so is the answer — an
    // absent section would leave the reader unable to tell "off" from "fine".
    assert!(markdown.contains("Sync disabled"));

    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["reminders_upcoming"], serde_json::json!([]));
    assert_eq!(json["sync"]["state"], "disabled");
}

#[test]
fn the_vitality_section_is_always_present() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "something", false);

    let markdown = render_markdown(&build_digest(&conn, 7).unwrap());

    // Vitality reads from a subsystem that does exist, so unlike reminders it
    // is reported even when the numbers are unremarkable.
    assert!(markdown.contains("## Vitality"));
    assert!(markdown.contains("Vault health"));
}

#[test]
fn the_window_is_configurable() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let old = add(&conn, "three weeks ago", false);
    backdate(&conn, &old, 21);

    assert_eq!(build_digest(&conn, 7).unwrap().recent_total, 0);
    assert_eq!(build_digest(&conn, 30).unwrap().recent_total, 1);
}

#[test]
fn a_deleted_memory_is_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "removed", false);
    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-01T00:00:00+00:00' WHERE id = ?",
        [&id],
    )
    .unwrap();

    assert_eq!(build_digest(&conn, 7).unwrap().recent_total, 0);
}
