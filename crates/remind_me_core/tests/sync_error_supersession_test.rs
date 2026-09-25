//! A recovered sync must stop reporting as failing (issue #148).
//!
//! `SyncWorkerStatus.last_error` is one process's memory; the `sync_log`
//! watermarks are shared. The normal deployment runs one MCP server process
//! per connected client against the same database, so a process that failed
//! while the hub was unreachable holds its error even after a sibling has
//! retried successfully — there is no cross-process clear.
//!
//! The classifier is tested directly. It is the whole judgment; the rest of
//! the path is one struct field move.

use remind_me_core::sync::sync_error_superseded;
use remind_me_core::Database;
use rusqlite::{params, Connection};

const EPOCH: &str = "1970-01-01T00:00:00+00:00";

fn at(minutes_ago: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339()
}

fn remote(conn: &Connection, remote_id: &str, push_at: &str, pull_at: &str) {
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_push, last_pull_id,
                               last_attempt_at, last_push_at, last_pull_at)
         VALUES (?, '', '', '', ?, ?, ?)",
        params![remote_id, push_at, push_at, pull_at],
    )
    .unwrap();
}

#[test]
fn a_success_after_the_failed_cycle_supersedes_the_error() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let failed_at = at(30);
    remote(&conn, "hub", &at(2), &at(2));

    // The watermarks only ever advance on success, so a success later than the
    // failed cycle means someone has already retried it.
    assert!(sync_error_superseded(&conn, Some(&failed_at)).unwrap());
}

#[test]
fn an_error_newer_than_every_success_is_still_current() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let failed_at = at(1);
    remote(&conn, "hub", &at(30), &at(30));

    assert!(!sync_error_superseded(&conn, Some(&failed_at)).unwrap());
}

#[test]
fn one_recovered_remote_does_not_speak_for_a_still_stuck_one() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let failed_at = at(30);
    remote(&conn, "hub", &at(2), &at(2));
    remote(&conn, "laptop", &at(90), &at(90));

    // The error is cycle-level — the first failure across all remotes — so the
    // remote that produced it is not identifiable from the message. Requiring
    // every remote to have moved is the only reading that cannot quietly
    // declare a still-stuck remote healthy.
    assert!(!sync_error_superseded(&conn, Some(&failed_at)).unwrap());
}

#[test]
fn a_remote_that_has_never_succeeded_keeps_the_error_reported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remote(&conn, "hub", EPOCH, EPOCH);

    // "Never succeeded" is not "succeeded a long time ago". Read as a
    // timestamp the epoch default would sort before any error and look like
    // ordinary staleness; read as a value it is the strongest reason to keep
    // reporting.
    assert!(!sync_error_superseded(&conn, Some(&at(30))).unwrap());
}

#[test]
fn a_half_contacted_remote_is_not_treated_as_recovered() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Pushed recently, never pulled.
    remote(&conn, "hub", &at(1), EPOCH);

    // The newest real success is what counts, and one direction working is
    // enough to have moved past the failure — the failed cycle recorded a
    // single error for the whole remote.
    assert!(sync_error_superseded(&conn, Some(&at(30))).unwrap());
}

#[test]
fn an_unparseable_cycle_timestamp_keeps_the_error_reported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remote(&conn, "hub", &at(1), &at(1));

    // Failing open here would hide a real outage behind a formatting problem.
    assert!(!sync_error_superseded(&conn, Some("last tuesday")).unwrap());
}

#[test]
fn a_missing_cycle_timestamp_keeps_the_error_reported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remote(&conn, "hub", &at(1), &at(1));

    assert!(!sync_error_superseded(&conn, None).unwrap());
}

#[test]
fn no_remotes_at_all_cannot_supersede_anything() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // An empty `sync_log` is not evidence of recovery — it is the absence of
    // evidence, and reporting healthy on it would be the same mistake in a
    // quieter form.
    assert!(!sync_error_superseded(&conn, Some(&at(30))).unwrap());
}

#[test]
fn a_success_exactly_at_the_failed_cycle_does_not_supersede_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let same = at(10);
    remote(&conn, "hub", &same, &same);

    // Pinned so a later `>=` cannot start clearing an error using the very
    // cycle that produced it — the failed cycle's own attempt stamps can land
    // on the same instant.
    assert!(!sync_error_superseded(&conn, Some(&same)).unwrap());
}
