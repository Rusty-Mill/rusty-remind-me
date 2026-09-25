//! Coverage for `remind_me_recalibrate_candidates`.
//!
//! Every clause of the heuristic gets a test that isolates it: a memory that
//! qualifies, and one differing only in that clause that does not. Testing the
//! predicate as a whole would pass just as happily with two of the three
//! conditions wired backwards.
//!
//! Rows are inserted directly rather than through `add_memory`, because the
//! conditions are all about *age* and a memory added now is 0 days old.

use remind_me_core::recalibrate::{
    candidates, RECALIBRATION_MIN_BASE_WEIGHT, RECALIBRATION_STALE_DAYS,
};
use remind_me_core::{Database, RecalibrateCandidatesInput, RecalibrateCandidatesResult};
use rusqlite::Connection;

/// A timestamp `days` in the past, in the schema's canonical format.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339()
}

/// A timestamp `seconds` in the past.
///
/// Used where a test needs a margin around the stale window that is smaller
/// than a day — see `the_stale_window_is_bracketed_on_both_sides` for why
/// day-granularity is too coarse to express the boundary safely.
fn seconds_ago(seconds: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339()
}

/// Insert a memory with full control over the three fields the heuristic reads.
///
/// `accessed_at` is `None` for a memory never retrieved, which the predicate
/// falls back to `created_at` for.
fn plant(
    conn: &Connection,
    id: &str,
    memory_type: &str,
    base_weight: f64,
    accessed_at: Option<&str>,
) {
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
                               created_at, updated_at, memory_type, base_weight,
                               accessed_at, access_count)
         VALUES (?, ?, 'general', '[]', 'manual', '{}', ?, ?, ?, ?, ?, 0)",
        rusqlite::params![
            id,
            format!("content of {}", id),
            days_ago(400),
            days_ago(400),
            memory_type,
            base_weight,
            accessed_at,
        ],
    )
    .unwrap();
}

fn run(conn: &Connection, limit: usize) -> RecalibrateCandidatesResult {
    candidates(conn, &RecalibrateCandidatesInput { limit }).unwrap()
}

fn ids(result: &RecalibrateCandidatesResult) -> Vec<String> {
    result.candidates.iter().map(|c| c.id.clone()).collect()
}

#[test]
fn a_stale_important_never_reviewed_memory_is_a_candidate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant(&conn, "mem_stale", "fact", 1.3, Some(&days_ago(200)));

    let result = run(&conn, 20);

    assert_eq!(ids(&result), vec!["mem_stale"]);
    assert_eq!(result.total_candidates, 1);
}

#[test]
fn a_recently_accessed_memory_is_not_a_candidate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Important and never reviewed, but still in active use — which is the
    // reference's whole argument for excluding it: a memory being retrieved is
    // presumably still classified correctly.
    plant(&conn, "mem_active", "fact", 1.3, Some(&days_ago(3)));

    let result = run(&conn, 20);

    assert!(result.candidates.is_empty());
    assert_eq!(result.total_candidates, 0);
}

#[test]
fn an_unimportant_memory_is_not_a_candidate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Old and unreviewed, but nothing ever suggested it mattered, so there is
    // no stale importance claim to re-examine.
    plant(&conn, "mem_dull", "action_item", 1.0, Some(&days_ago(300)));

    let result = run(&conn, 20);

    assert!(result.candidates.is_empty());
}

#[test]
fn importance_counts_from_either_direction() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // A durable type whose weight never moved, and a weight that was raised on
    // a type that implies nothing. Each satisfies one half of the OR alone.
    plant(&conn, "mem_by_type", "decision", 1.0, Some(&days_ago(300)));
    plant(
        &conn,
        "mem_by_weight",
        "action_item",
        RECALIBRATION_MIN_BASE_WEIGHT,
        Some(&days_ago(300)),
    );

    let result = run(&conn, 20);

    let mut found = ids(&result);
    found.sort();
    assert_eq!(found, vec!["mem_by_type", "mem_by_weight"]);
}

#[test]
fn a_memory_that_received_feedback_is_not_a_candidate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant(&conn, "mem_reviewed", "fact", 1.3, Some(&days_ago(300)));
    conn.execute(
        "INSERT INTO memory_feedback
             (id, memory_id, query, query_tokens, signal, magnitude, created_at)
         VALUES ('fb_1', 'mem_reviewed', 'anything', '[\"anything\"]', 'helpful', 0.1, ?)",
        rusqlite::params![days_ago(250)],
    )
    .unwrap();

    let result = run(&conn, 20);

    // Feedback stands in for "has actually been looked at" — the reference's
    // own proxy. Re-surfacing a memory somebody already judged wastes the
    // reviewer's attention on the one thing they have already spent it on.
    assert!(result.candidates.is_empty());
}

#[test]
fn a_never_accessed_memory_falls_back_to_its_creation_date() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // accessed_at NULL is the common case for exactly the memories this tool
    // is for: written once, never retrieved since. Without the COALESCE the
    // date comparison would be NULL and the row would silently never qualify.
    plant(&conn, "mem_untouched", "fact", 1.3, None);

    let result = run(&conn, 20);

    assert_eq!(ids(&result), vec!["mem_untouched"]);
    assert!(result.candidates[0].accessed_at.is_none());
}

#[test]
fn a_deleted_or_superseded_memory_is_not_a_candidate() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant(&conn, "mem_gone", "fact", 1.3, Some(&days_ago(300)));
    plant(&conn, "mem_replaced", "fact", 1.3, Some(&days_ago(300)));
    plant(&conn, "mem_live", "fact", 1.3, Some(&days_ago(300)));
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = 'mem_gone'",
        rusqlite::params![days_ago(10)],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_live' WHERE id = 'mem_replaced'",
        [],
    )
    .unwrap();

    let result = run(&conn, 20);

    assert_eq!(ids(&result), vec!["mem_live"]);
}

/// The stale window is pinned from both sides, with a margin.
///
/// # Why not plant exactly at the threshold
///
/// An earlier version of this test planted `days_ago(RECALIBRATION_STALE_DAYS)`
/// exactly, reasoning that only an exact-boundary stamp distinguishes the
/// predicate's `>=` from a `>`. That is true, and it is also **not reachable**:
/// the predicate compares against `julianday('now')`, which SQLite evaluates at
/// coarser precision than the sub-second stamp `days_ago` produces. The
/// difference therefore lands just *below* the threshold about half the time.
///
/// Measured over 2000 samples of the exact arithmetic: `diff < 90` in 993 of
/// them, typical shortfall `-1.16e-08` days. So that version was a ~50%
/// coin flip that failed on unrelated PRs (issue #180) while catching nothing
/// reliably — a `>=` → `>` regression would have flipped it from "fails half
/// the time" to "fails half the time".
///
/// Bracketing instead pins the threshold to within an hour and never races.
/// Two seconds is far larger than any plausible clock or precision skew and far
/// smaller than the day-granularity the window is expressed in.
#[test]
fn the_stale_window_is_bracketed_on_both_sides() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // Just past the window: must qualify.
    plant(
        &conn,
        "mem_stale",
        "fact",
        1.3,
        Some(&seconds_ago(RECALIBRATION_STALE_DAYS * 86_400 + 2)),
    );
    // Just inside it: must not.
    plant(
        &conn,
        "mem_fresh",
        "fact",
        1.3,
        Some(&seconds_ago(RECALIBRATION_STALE_DAYS * 86_400 - 3_600)),
    );

    assert_eq!(ids(&run(&conn, 20)), vec!["mem_stale"]);
}

#[test]
fn total_candidates_counts_past_the_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        plant(
            &conn,
            &format!("mem_{}", i),
            "fact",
            1.3,
            Some(&days_ago(300)),
        );
    }

    let result = run(&conn, 2);

    // The whole point of the count is telling a caller how much is left behind
    // the page, so it must not be derived from the page.
    assert_eq!(result.candidates.len(), 2);
    assert_eq!(result.total_candidates, 5);
}

#[test]
fn candidates_are_ordered_by_importance_then_staleness() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant(&conn, "mem_low", "fact", 1.15, Some(&days_ago(300)));
    plant(&conn, "mem_high", "fact", 1.30, Some(&days_ago(300)));
    plant(&conn, "mem_high_older", "fact", 1.30, Some(&days_ago(365)));

    let result = run(&conn, 20);

    // base_weight DESC, then accessed_at ASC: the most important first, and
    // among equals the one untouched longest.
    assert_eq!(
        ids(&result),
        vec!["mem_high_older", "mem_high", "mem_low"],
        "a reviewer with time for three should see the three that matter most"
    );
}

#[test]
fn an_empty_store_is_an_empty_batch_not_an_error() {
    let db = Database::open_in_memory().unwrap();

    let result = run(&db.conn(), 20);

    assert!(result.candidates.is_empty());
    assert_eq!(result.total_candidates, 0);
}
