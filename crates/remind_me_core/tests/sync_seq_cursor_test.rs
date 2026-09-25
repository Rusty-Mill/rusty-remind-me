//! The client half of the hub-sequence pull cursor (reference issue #167).
//!
//! The hub has served `since_seq` since the sequence column was added, but a
//! client that never sends it leaves the bug the sequence exists to fix fully
//! live: a node back online after a fortnight pushes records still stamped
//! with old `updated_at` values, which sort *behind* every other node's
//! already-advanced legacy cursor and are therefore permanently invisible.
//!
//! These tests drive `pull_remote` against a scripted HTTP stub rather than a
//! real peer, because the whole point is what the client *sends* and how it
//! reacts to what comes back — including responses no real server in this
//! workspace produces (a hub that omits `hub_seq`, an empty first page).

use remind_me_core::sync::pull_remote;
use remind_me_core::Database;
use rusqlite::Connection;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread::JoinHandle;

const SECRET: &str = "test-secret";

/// How long to wait for a request the client is expected to make.
///
/// Every wait in this file is bounded. A blocking `recv()` here does not fail
/// when the client stops sending the request under test — it *hangs*, and a
/// hung test is strictly worse than a failing one: CI reports a timeout with
/// no failing assertion to read. Verified by sabotage, where disabling the
/// sequence cursor entirely made an unbounded version wait forever instead of
/// reporting the regression.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Receive the next request target, failing with `context` rather than hanging.
fn next_request(seen: &mpsc::Receiver<String>, context: &str) -> String {
    seen.recv_timeout(REQUEST_TIMEOUT)
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}

/// Serve `responses` in order, recording each request's target (path and query
/// string) so a test can assert on the cursor the client actually sent.
///
/// The recorded targets come back over a channel rather than from the join
/// handle: a test that asserts on request *n* should not have to wait for the
/// server to finish serving every scripted response first, and several of
/// these deliberately leave responses unconsumed.
fn stub_hub(responses: Vec<String>) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for body in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            // "GET /sync/pull?since_seq=0&limit=1 HTTP/1.1"
            let target = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            // Drain the rest of the head so the client's write completes.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send(target);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{}", port), rx, handle)
}

fn records(body: serde_json::Value) -> String {
    body.to_string()
}

/// A stub that answers based on the cursor it was sent, the way a real hub
/// does: `reply(target) -> body`.
///
/// Needed for the end-to-end test, where a stub that returns the same records
/// whichever cursor arrives would pass even with the whole feature disabled —
/// the record has to be genuinely unreachable by the legacy cursor for the
/// test to mean anything.
fn scripted_hub(
    reply: impl Fn(&str) -> String + Send + 'static,
    max_requests: usize,
) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for _ in 0..max_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            let target = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let body = reply(&target);
            let _ = tx.send(target);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{}", port), rx, handle)
}

fn seq_cursor(conn: &Connection, remote_id: &str) -> i64 {
    conn.query_row(
        "SELECT last_pull_seq FROM sync_log WHERE remote_id = ?",
        [remote_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// A record carrying a `hub_seq`, as a hub serves it.
fn hub_record(id: &str, updated_at: &str, hub_seq: i64) -> serde_json::Value {
    json!({
        "id": id,
        "content": format!("content for {id}"),
        "created_at": updated_at,
        "updated_at": updated_at,
        "hub_seq": hub_seq,
    })
}

/// The same record as a peer serves it — no sequence to report.
fn peer_record(id: &str, updated_at: &str) -> serde_json::Value {
    json!({
        "id": id,
        "content": format!("content for {id}"),
        "created_at": updated_at,
        "updated_at": updated_at,
    })
}

// ---------------------------------------------------------------------------
// Establishing the cursor
// ---------------------------------------------------------------------------

#[test]
fn a_record_carrying_hub_seq_establishes_the_sequence_cursor() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (url, seen, _handle) = stub_hub(vec![
        // The probe.
        records(json!({"records": [hub_record("m1", "2026-01-01T00:00:00+00:00", 7)]})),
        // The first real page.
        records(json!({"records": [hub_record("m1", "2026-01-01T00:00:00+00:00", 7)]})),
    ]);

    pull_remote(&conn, &url, SECRET, "this-node", "hub").unwrap();

    let probe = next_request(&seen, "the hub_seq probe");
    assert!(
        probe.contains("since_seq=0") && probe.contains("limit=1"),
        "the probe must ask for one record from the start of the sequence, got {probe}"
    );
    let first_page = next_request(&seen, "the first page request");
    assert!(
        first_page.contains("since_seq=0"),
        "having established support, the first page must pull by sequence, got {first_page}"
    );
    assert!(
        !first_page.contains("since_id="),
        "the legacy cursor must not be sent once the sequence cursor is live, got {first_page}"
    );
    assert_eq!(
        seq_cursor(&conn, "hub"),
        7,
        "cursor advances to the greatest hub_seq applied"
    );
}

#[test]
fn a_200_without_hub_seq_marks_the_remote_unsupported() {
    // The load-bearing case: a remote predating the feature ignores the
    // unknown `since_seq` parameter and answers happily from its legacy
    // cursor. A 200 therefore proves nothing — only the field's presence does.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (url, seen, _handle) = stub_hub(vec![
        records(json!({"records": [peer_record("m1", "2026-01-01T00:00:00+00:00")]})),
        records(json!({"records": [peer_record("m1", "2026-01-01T00:00:00+00:00")]})),
    ]);

    pull_remote(&conn, &url, SECRET, "this-node", "peer").unwrap();

    let _probe = next_request(&seen, "the hub_seq probe");
    let first_page = next_request(&seen, "the first page request");
    assert_eq!(seq_cursor(&conn, "peer"), -2, "SEQ_UNSUPPORTED, and sticky");
    assert!(
        first_page.contains("since=") && first_page.contains("since_id="),
        "an unsupported remote must stay on the legacy cursor, got {first_page}"
    );
    assert!(
        !first_page.contains("since_seq="),
        "and must not be sent a sequence cursor, got {first_page}"
    );
}

#[test]
fn an_empty_probe_leaves_the_state_unknown_rather_than_unsupported() {
    // An empty hub returns no records whichever cursor it understands, so an
    // empty result is not evidence of absence. Marking it unsupported here
    // would be sticky and wrong, and only `sync_repair` would clear it.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (url, _seen, _handle) = stub_hub(vec![
        records(json!({"records": []})),
        records(json!({"records": []})),
    ]);

    pull_remote(&conn, &url, SECRET, "this-node", "empty-hub").unwrap();

    let stored: Option<i64> = conn
        .query_row(
            "SELECT last_pull_seq FROM sync_log WHERE remote_id = ?",
            ["empty-hub"],
            |r| r.get(0),
        )
        .ok();
    assert!(
        stored.is_none() || stored == Some(-1),
        "an empty probe must leave the cursor unknown, got {stored:?}"
    );
}

#[test]
fn an_unreachable_remote_is_not_mistaken_for_one_lacking_the_feature() {
    // Bind and immediately drop, so the port is closed: the probe fails at the
    // transport, which must not be recorded as "does not support".
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let url = format!("http://127.0.0.1:{port}");

    let _ = pull_remote(&conn, &url, SECRET, "this-node", "down");

    let stored: Option<i64> = conn
        .query_row(
            "SELECT last_pull_seq FROM sync_log WHERE remote_id = ?",
            ["down"],
            |r| r.get(0),
        )
        .ok();
    assert!(
        stored.is_none() || stored == Some(-1),
        "a failed probe must leave the state unknown so the next cycle retries, got {stored:?}"
    );
}

// ---------------------------------------------------------------------------
// The bug the cursor exists to fix
// ---------------------------------------------------------------------------

#[test]
fn a_record_stamped_behind_the_legacy_cursor_is_still_pulled() {
    // The whole point, end to end. The legacy cursor is already advanced to
    // 2026; a node back online after a fortnight pushes a record still stamped
    // 2025, which the hub assigns a high `hub_seq`. Under the legacy cursor it
    // sorts behind and is invisible forever. Under the sequence cursor it
    // arrives.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_pull_id) VALUES (?, ?, '')",
        rusqlite::params!["hub", "2026-06-01T00:00:00+00:00"],
    )
    .unwrap();

    // The stub behaves like a real hub: the stranded record is reachable
    // *only* by sequence. Ask by the legacy cursor and it sorts behind
    // `since=2026-06-01`, so the hub returns nothing — which is exactly the
    // invisibility being fixed. Without this the test would pass with the
    // feature switched off.
    let (url, _seen, _handle) = scripted_hub(
        |target| {
            if target.contains("since_seq=") {
                records(json!({"records": [
                    hub_record("stranded", "2025-01-01T00:00:00+00:00", 900)
                ]}))
            } else {
                records(json!({"records": []}))
            }
        },
        4,
    );

    let report = pull_remote(&conn, &url, SECRET, "this-node", "hub").unwrap();

    assert_eq!(report.applied, 1, "the stranded record must be applied");
    let content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = 'stranded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content, "content for stranded");
    assert_eq!(
        seq_cursor(&conn, "hub"),
        900,
        "and the sequence cursor advances past it"
    );
}

#[test]
fn the_cursor_advances_to_the_greatest_hub_seq_in_a_page() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let page = json!({"records": [
        hub_record("a", "2026-01-01T00:00:00+00:00", 11),
        hub_record("b", "2026-01-02T00:00:00+00:00", 44),
        hub_record("c", "2026-01-03T00:00:00+00:00", 22),
    ]});
    let (url, _seen, _handle) = stub_hub(vec![
        records(page.clone()),
        records(page),
        records(json!({"records": []})),
    ]);

    pull_remote(&conn, &url, SECRET, "this-node", "hub").unwrap();

    assert_eq!(seq_cursor(&conn, "hub"), 44, "greatest, not last or first");
}

#[test]
fn a_page_that_does_not_advance_the_sequence_stops_the_cycle() {
    // A remote replaying the same page must not trap a pull cycle. Three
    // responses are scripted; only the probe and one page should be consumed.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_pull_seq) VALUES (?, ?, ?)",
        rusqlite::params!["hub", "1970-01-01T00:00:00+00:00", 50],
    )
    .unwrap();

    let stale = json!({"records": [hub_record("old", "2026-01-01T00:00:00+00:00", 50)]});
    let (url, seen, _handle) = stub_hub(vec![
        records(stale.clone()),
        records(stale.clone()),
        records(stale),
    ]);

    pull_remote(&conn, &url, SECRET, "this-node", "hub").unwrap();

    // No probe: the cursor was already established at 50.
    let first = next_request(&seen, "the first page request");
    assert!(first.contains("since_seq=50"), "got {first}");
    assert!(
        seen.recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "a page that does not advance the cursor must end the cycle, not re-request"
    );
    assert_eq!(seq_cursor(&conn, "hub"), 50, "and the cursor stays put");
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

#[test]
fn sync_repair_clears_a_stuck_unsupported_verdict() {
    // The documented path after upgrading a hub: `SEQ_UNSUPPORTED` is sticky
    // by design, so without this a hub that gained the feature would never be
    // re-probed. Back to unknown rather than to 0, which would assert support
    // that was never established.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_pull_id, last_pull_seq)
         VALUES ('hub', '2026-01-01T00:00:00+00:00', 'some-id', -2)",
        [],
    )
    .unwrap();

    assert!(remind_me_core::sync::sync_repair(&conn, "hub").unwrap());

    assert_eq!(seq_cursor(&conn, "hub"), -1, "back to SEQ_UNKNOWN, not 0");
    let (last_pull, last_pull_id): (String, String) = conn
        .query_row(
            "SELECT last_pull, last_pull_id FROM sync_log WHERE remote_id = 'hub'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(last_pull, "1970-01-01T00:00:00+00:00");
    assert_eq!(last_pull_id, "");
}
