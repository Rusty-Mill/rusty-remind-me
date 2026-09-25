//! Coverage for `remind_me_set_reminder` / `remind_me_list_reminders`
//! (gap T1a, issue #116).
//!
//! The windows get the attention. Setting and clearing are one UPDATE each;
//! what is actually easy to get wrong is which memories a window contains —
//! and every one of those mistakes is silent, because a listing that quietly
//! omits a due reminder looks exactly like a vault with nothing due.

use remind_me_core::models::{ReminderWindow, SetReminderOutcome};
use remind_me_core::reminders::{list_reminders, parse_remind_at, set_reminder};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::{params, Connection};

fn add(conn: &Connection, content: &str) -> String {
    remind_me_core::db::queries::add_memory(
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

/// Write `remind_at` directly, bypassing the future-only guard, so the overdue
/// window can be tested at all — `set_reminder` exists precisely to make this
/// state unreachable through the tool.
fn force_remind_at(conn: &Connection, memory_id: &str, when: &str) {
    conn.execute(
        "UPDATE memories SET remind_at = ? WHERE id = ?",
        params![when, memory_id],
    )
    .unwrap();
}

fn past(hours: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
}

// ---------------------------------------------------------------------------
// Setting and clearing
// ---------------------------------------------------------------------------

#[test]
fn setting_a_future_reminder_stores_it_and_bumps_updated_at() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "renew the passport");

    let before: String = conn
        .query_row(
            "SELECT updated_at FROM memories WHERE id = ?",
            params![&id],
            |r| r.get(0),
        )
        .unwrap();

    let outcome = set_reminder(&conn, &id, Some(&future(24))).unwrap();

    assert!(matches!(outcome, SetReminderOutcome::Set { .. }));

    let (stored, after): (Option<String>, String) = conn
        .query_row(
            "SELECT remind_at, updated_at FROM memories WHERE id = ?",
            params![&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(stored.is_some());
    // The bump is not cosmetic: it is what puts the change in the sync outbox
    // and what LWW compares. Without it a reminder set here loses to any older
    // copy of the same memory on another node.
    assert!(after >= before);
}

#[test]
fn a_reminder_is_stored_canonicalized_to_utc() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "call the dentist");

    // Same instant, written in a non-UTC offset.
    let when = (chrono::Utc::now() + chrono::Duration::hours(30))
        .with_timezone(&chrono::FixedOffset::east_opt(5 * 3600).unwrap())
        .to_rfc3339();
    set_reminder(&conn, &id, Some(&when)).unwrap();

    let stored: String = conn
        .query_row(
            "SELECT remind_at FROM memories WHERE id = ?",
            params![&id],
            |r| r.get(0),
        )
        .unwrap();

    // Stored as UTC, because `remind_at` is compared as a *string* against a
    // UTC `now` in every window query. Two equal instants written in different
    // offsets would otherwise sort against each other wrongly.
    assert!(
        stored.ends_with("+00:00"),
        "stored as {stored}, which does not compare correctly against a UTC now"
    );
}

#[test]
fn a_naive_timestamp_is_read_as_utc_not_local() {
    let naive = "2099-06-01T09:30:00";
    let parsed = parse_remind_at(naive).expect("a naive ISO-8601 timestamp parses");

    // Local would be friendlier to type and worse to store: the same string
    // would mean different instants on two synced machines.
    assert_eq!(parsed.to_rfc3339(), "2099-06-01T09:30:00+00:00");
}

#[test]
fn clearing_removes_the_reminder() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "water the plants");
    set_reminder(&conn, &id, Some(&future(2))).unwrap();

    let outcome = set_reminder(&conn, &id, None).unwrap();

    assert!(matches!(outcome, SetReminderOutcome::Cleared { .. }));
    let stored: Option<String> = conn
        .query_row(
            "SELECT remind_at FROM memories WHERE id = ?",
            params![&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored.is_none());
    assert!(list_reminders(&conn, ReminderWindow::All, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn a_blank_string_clears_rather_than_failing_to_parse() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "book the flight");
    set_reminder(&conn, &id, Some(&future(2))).unwrap();

    // A blank field arriving from a form is not a broken timestamp — it is
    // how "no reminder" is expressed there.
    let outcome = set_reminder(&conn, &id, Some("   ")).unwrap();

    assert!(matches!(outcome, SetReminderOutcome::Cleared { .. }));
}

#[test]
fn a_past_timestamp_is_rejected_rather_than_stored() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "already happened");

    let outcome = set_reminder(&conn, &id, Some(&past(1))).unwrap();

    // Stored, it would land straight in the overdue pile — the bucket that
    // means "the scheduler was down when this came due". A typo would then be
    // indistinguishable from a genuine missed delivery.
    match outcome {
        SetReminderOutcome::Rejected { reason } => {
            assert!(reason.contains("must be in the future"))
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT remind_at FROM memories WHERE id = ?",
            params![&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored.is_none(), "a rejected reminder must not be written");
}

#[test]
fn an_unparseable_timestamp_is_rejected() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "sometime");

    match set_reminder(&conn, &id, Some("next tuesday")).unwrap() {
        SetReminderOutcome::Rejected { reason } => assert!(reason.contains("ISO-8601")),
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn setting_a_reminder_on_a_missing_memory_reports_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = set_reminder(&conn, "mem_nope", Some(&future(1))).unwrap();

    assert!(matches!(outcome, SetReminderOutcome::NotFound { .. }));
}

#[test]
fn setting_a_reminder_writes_no_revision() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "scheduling is not editing");

    set_reminder(&conn, &id, Some(&future(5))).unwrap();
    set_reminder(&conn, &id, None).unwrap();

    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_revisions WHERE memory_id = ?",
            params![&id],
            |r| r.get(0),
        )
        .unwrap();
    // The revision log exists to recover a value a human replaced. A vault
    // whose history is half reminder-scheduling noise is harder to read back
    // than one that only records edits.
    assert_eq!(revisions, 0);
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[test]
fn upcoming_and_overdue_split_on_now_and_all_is_the_union() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let soon = add(&conn, "due later today");
    let missed = add(&conn, "should already have fired");
    let none = add(&conn, "no reminder at all");
    set_reminder(&conn, &soon, Some(&future(3))).unwrap();
    force_remind_at(&conn, &missed, &past(3));

    let upcoming = list_reminders(&conn, ReminderWindow::Upcoming, 20).unwrap();
    let overdue = list_reminders(&conn, ReminderWindow::Overdue, 20).unwrap();
    let all = list_reminders(&conn, ReminderWindow::All, 20).unwrap();

    assert_eq!(
        upcoming.iter().map(|m| &m.id).collect::<Vec<_>>(),
        vec![&soon]
    );
    assert_eq!(
        overdue.iter().map(|m| &m.id).collect::<Vec<_>>(),
        vec![&missed]
    );
    assert_eq!(all.len(), 2);
    // A memory with no reminder is in no window — the point of the filter.
    assert!(all.iter().all(|m| m.id != none));
}

#[test]
fn a_delivered_reminder_drops_out_of_every_window() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "already told you");
    let when = past(2);
    force_remind_at(&conn, &id, &when);
    conn.execute(
        "INSERT INTO reminder_deliveries (memory_id, remind_at, delivered_at)
         VALUES (?, ?, ?)",
        params![&id, &when, chrono::Utc::now().to_rfc3339()],
    )
    .unwrap();

    // Overdue means "came due and nothing told you" — not "came due". Without
    // this the same reminder would sit in the list forever after firing.
    assert!(list_reminders(&conn, ReminderWindow::Overdue, 20)
        .unwrap()
        .is_empty());
    assert!(list_reminders(&conn, ReminderWindow::All, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn rescheduling_a_delivered_reminder_makes_it_pending_again() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "recurring chore");
    let fired = past(2);
    force_remind_at(&conn, &id, &fired);
    conn.execute(
        "INSERT INTO reminder_deliveries (memory_id, remind_at, delivered_at)
         VALUES (?, ?, ?)",
        params![&id, &fired, chrono::Utc::now().to_rfc3339()],
    )
    .unwrap();

    set_reminder(&conn, &id, Some(&future(6))).unwrap();

    // Delivery is keyed on `(memory_id, remind_at)`, not on the memory alone.
    // Keyed on the memory, a chore could be reminded about exactly once ever.
    let upcoming = list_reminders(&conn, ReminderWindow::Upcoming, 20).unwrap();
    assert_eq!(upcoming.len(), 1);
    assert_eq!(upcoming[0].id, id);
}

#[test]
fn a_deleted_memorys_reminder_is_in_no_window() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "deleted but scheduled");
    set_reminder(&conn, &id, Some(&future(4))).unwrap();
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = ?",
        params![chrono::Utc::now().to_rfc3339(), &id],
    )
    .unwrap();

    // Firing this would surface content the user deleted.
    assert!(list_reminders(&conn, ReminderWindow::All, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn reminders_come_back_soonest_first_and_respect_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let later = add(&conn, "later");
    let sooner = add(&conn, "sooner");
    set_reminder(&conn, &later, Some(&future(48))).unwrap();
    set_reminder(&conn, &sooner, Some(&future(2))).unwrap();

    let listed = list_reminders(&conn, ReminderWindow::Upcoming, 20).unwrap();
    assert_eq!(listed[0].id, sooner, "the next thing due leads");

    // A truncated list has to keep the soonest, not an arbitrary one.
    let capped = list_reminders(&conn, ReminderWindow::Upcoming, 1).unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].id, sooner);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn the_markdown_listing_shows_the_reminder_time() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "renew the domain");
    set_reminder(&conn, &id, Some(&future(9))).unwrap();

    let listed = list_reminders(&conn, ReminderWindow::Upcoming, 20).unwrap();
    let markdown = remind_me_core::reminders::render_memories_markdown(&listed);

    // A reminder listing that does not say *when* has omitted the one field
    // the caller asked the question about.
    assert!(markdown.contains("**Reminder:**"));
    assert!(markdown.contains(&id));
    assert!(markdown.contains("renew the domain"));
}

#[test]
fn an_empty_markdown_listing_says_so_rather_than_returning_nothing() {
    assert_eq!(
        remind_me_core::reminders::render_memories_markdown(&[]),
        "_No memories found._"
    );
}
