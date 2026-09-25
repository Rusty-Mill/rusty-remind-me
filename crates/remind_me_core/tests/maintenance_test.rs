//! Maintenance nudges and capture health (gap E4, issue #151).
//!
//! The throttle and the selection get the attention. A nudge that fires too
//! often is noise a reader learns to skip, and one that never fires is a
//! backlog nobody sees — and both look identical from inside the code that
//! produces them.

use remind_me_core::maintenance::{
    capture_health, due, pending_counts, render_notice, reset_throttle, NUDGE_MAX_QUEUES,
    NUDGE_THRESHOLD,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// Serialises the throttle tests: the timer map is process-wide, so two of
/// them running concurrently would each see the other's claim.
static THROTTLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

fn counts(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

// ---------------------------------------------------------------------------
// Counts
// ---------------------------------------------------------------------------

#[test]
fn an_empty_vault_has_every_queue_at_zero() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let counts = pending_counts(&conn);

    assert!(!counts.is_empty(), "every queue should be reported");
    assert!(
        counts.values().all(|c| *c == 0),
        "expected all zero, got {counts:?}"
    );
}

#[test]
fn an_unclassified_memory_lands_in_that_queue() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a memory with no classification");

    assert_eq!(pending_counts(&conn)["unclassified_memories"], 1);
}

#[test]
fn a_deleted_memory_is_in_no_queue() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "about to be deleted");
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = ?",
        params![chrono::Utc::now().to_rfc3339(), &id],
    )
    .unwrap();

    // Nudging someone to classify a memory they deleted is work that cannot
    // be done and would never clear.
    let counts = pending_counts(&conn);
    assert_eq!(counts["unclassified_memories"], 0);
    assert_eq!(counts["unannotated_memories"], 0);
}

#[test]
fn a_broken_queue_reports_zero_rather_than_breaking_the_caller() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Simulates a partially-migrated database: the table a queue needs is
    // gone. A status helper must not be the thing that breaks a search.
    conn.execute("DROP TABLE memory_entities", []).unwrap();

    let counts = pending_counts(&conn);

    assert_eq!(counts["unannotated_memories"], 0);
    // The rest still report honestly rather than the whole call failing.
    assert!(counts.contains_key("unclassified_memories"));
}

// ---------------------------------------------------------------------------
// Capture health
// ---------------------------------------------------------------------------

#[test]
fn never_configured_is_visible_rather_than_inferred() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "an ordinary memory, not a capture");

    let health = capture_health(&conn);

    // A client where auto-capture was never set up is indistinguishable from
    // one where it was but nothing was worth capturing — both are silent.
    // `ever_captured` is what separates them.
    assert!(!health.ever_captured);
    assert_eq!(health.captures, 0);
    assert!(health.last_capture_at.is_none());
}

#[test]
fn one_capture_counts_once_even_though_it_writes_two_rows() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for content in ["the dialog", "the summary"] {
        let id = add(&conn, content);
        conn.execute(
            "UPDATE memories SET capture_id = 'cap_1' WHERE id = ?",
            params![&id],
        )
        .unwrap();
    }

    let health = capture_health(&conn);

    // Counted by row, a single capture would read as two and the number would
    // silently overstate how much is being captured.
    assert_eq!(health.captures, 1);
    assert!(health.ever_captured);
    assert!(health.last_capture_at.is_some());
}

// ---------------------------------------------------------------------------
// Notice selection
// ---------------------------------------------------------------------------

#[test]
fn nothing_over_the_threshold_produces_no_notice() {
    let below = NUDGE_THRESHOLD - 1;
    assert!(render_notice(&counts(&[("unclassified_memories", below)])).is_none());
}

#[test]
fn the_threshold_is_inclusive() {
    // Pinned in both directions so a later `>` cannot quietly raise the bar by
    // one and make a genuinely-due nudge stop firing.
    assert!(render_notice(&counts(&[("unclassified_memories", NUDGE_THRESHOLD)])).is_some());
    assert!(render_notice(&counts(&[("unclassified_memories", NUDGE_THRESHOLD - 1)])).is_none());
}

#[test]
fn the_deepest_backlogs_come_first() {
    let notice = render_notice(&counts(&[
        ("unclassified_memories", 30),
        ("unannotated_memories", 900),
        ("unnormalized_imports", 100),
    ]))
    .unwrap();

    let lines: Vec<&str> = notice.lines().skip(1).collect();
    assert!(lines[0].contains("900"), "got {lines:?}");
    assert!(lines[1].contains("100"), "got {lines:?}");
    assert!(lines[2].contains("30"), "got {lines:?}");
}

#[test]
fn at_most_three_backlogs_are_named() {
    let notice = render_notice(&counts(&[
        ("unclassified_memories", 100),
        ("unannotated_memories", 200),
        ("unnormalized_imports", 300),
        ("undecomposed_captures", 400),
        ("contradiction_candidates", 500),
    ]))
    .unwrap();

    // A list of every queue is a wall of text nobody acts on.
    assert_eq!(notice.lines().count(), NUDGE_MAX_QUEUES + 1, "{notice}");
}

#[test]
fn equal_backlogs_do_not_reshuffle_between_calls() {
    let equal = counts(&[
        ("unannotated_memories", 50),
        ("unclassified_memories", 50),
        ("unnormalized_imports", 50),
    ]);

    // A nudge whose order changes for no reason reads as new information.
    let first = render_notice(&equal).unwrap();
    for _ in 0..5 {
        assert_eq!(render_notice(&equal).unwrap(), first);
    }
}

#[test]
fn each_line_names_the_prompt_that_drains_that_queue() {
    let notice = render_notice(&counts(&[
        ("unclassified_memories", 40),
        ("contradiction_candidates", 60),
    ]))
    .unwrap();

    // Telling someone a backlog exists without telling them how to drain it
    // is a complaint, not a nudge.
    assert!(notice.contains("`classify_memories` prompt"), "{notice}");
    assert!(
        notice.contains("`review_contradictions` prompt"),
        "{notice}"
    );
}

#[test]
fn an_unknown_queue_key_degrades_rather_than_panicking() {
    let notice = render_notice(&counts(&[("some_future_queue", 99)])).unwrap();

    // Naming a queue oddly is cosmetic; crashing a search over one is not.
    assert!(notice.contains("some_future_queue"), "{notice}");
}

// ---------------------------------------------------------------------------
// Throttle
// ---------------------------------------------------------------------------

#[test]
fn the_first_check_is_due_and_the_second_is_not() {
    let _guard = THROTTLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_throttle();

    assert!(due("test_first", 3600));
    assert!(!due("test_first", 3600));
}

#[test]
fn a_zero_interval_is_always_due() {
    let _guard = THROTTLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_throttle();

    assert!(due("test_zero", 0));
    assert!(due("test_zero", 0), "an elapsed >= 0 interval never blocks");
}

#[test]
fn separate_timers_do_not_silence_each_other() {
    let _guard = THROTTLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_throttle();

    assert!(due("advisory_a", 3600));
    // Keyed rather than global: two independent advisories with different
    // cadences must not compete for one slot, or whichever fires first
    // silences the other for an hour.
    assert!(due("advisory_b", 3600));
    assert!(!due("advisory_a", 3600));
}

#[test]
fn the_slot_is_claimed_even_when_nothing_is_reported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let _guard = THROTTLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_throttle();

    // An empty vault produces no notice — but the check must still have cost
    // its slot, or a quiet vault would re-run every count on every search.
    assert!(remind_me_core::maintenance::maybe_notice(&conn).is_none());
    assert!(
        !due("maintenance", 3600),
        "the slot was not claimed, so the counts would re-run on the next call"
    );
}
