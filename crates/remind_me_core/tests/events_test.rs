//! Automation event stream (gap E5, issue #152).
//!
//! Asserted against a real listening socket rather than by inspecting the
//! payload builder alone. The guarantee that matters — that memory content
//! never reaches the wire — is a property of what actually gets POSTed, and a
//! test that only reads `payload()` would keep passing if a later change
//! started sending the whole memory from the emit path.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::events::{self, Event};
use remind_me_core::{Database, MemoryAddInput};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// The event webhook URL is a process-wide env var, so these run one at a time.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A one-shot HTTP server that captures `count` request bodies.
fn capture(count: usize) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/events", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&body).to_string());
        }
    });

    (url, rx)
}

fn add(conn: &rusqlite::Connection, content: &str, category: &str) -> String {
    remind_me_core::db::queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            tags: vec!["secret-tag".into()],
            source: "manual".into(),
            metadata: serde_json::json!({ "confidential": "value" }),
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

fn received(rx: &mpsc::Receiver<String>) -> serde_json::Value {
    let body = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("expected an event POST");
    serde_json::from_str(&body).expect("event body should be JSON")
}

// ---------------------------------------------------------------------------
// The payload contract
// ---------------------------------------------------------------------------

#[test]
fn the_payload_carries_no_memory_content() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, rx) = capture(1);
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &url);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "the nuclear launch codes are 0000", "general");
    events::drain();

    let event = received(&rx);

    // This is an event-notification stream, not a content-sync mechanism.
    // Content on the wire would silently turn every configured webhook into an
    // egress path for the whole vault, with no per-call intent to check.
    let raw = event.to_string();
    assert!(!raw.contains("nuclear"), "content reached the wire: {raw}");
    assert!(!raw.contains("secret-tag"), "tags reached the wire: {raw}");
    assert!(!raw.contains("confidential"), "metadata reached: {raw}");

    // What a consumer does get: enough to call back for the rest.
    assert_eq!(event["event"], "created");
    assert_eq!(event["memory_id"], id);
    assert_eq!(event["category"], "general");
    assert!(event["timestamp"].is_string());

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

#[test]
fn each_mutation_kind_emits_its_own_event() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, rx) = capture(3);
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &url);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "something", "general");
    remind_me_core::db::queries::update_memory(
        &conn,
        &remind_me_core::models::MemoryUpdateInput {
            memory_id: id.clone(),
            clear_superseded: false,
            content: Some("something else".into()),
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
        },
    )
    .unwrap();
    remind_me_core::db::queries::delete_memory(&conn, &id).unwrap();
    events::drain();

    let mut kinds: Vec<String> = (0..3)
        .map(|_| received(&rx)["event"].as_str().unwrap().to_string())
        .collect();

    // Compared as a multiset, not a sequence (#210). `emit` posts each event on
    // its own thread, so three mutations in a row race to the socket and the
    // arrival order is whatever the scheduler picks — sorting drops a claim the
    // system never made while keeping the one it did: each mutation emits
    // exactly one event, of the right kind, and no extras. Sorting rather than
    // de-duplicating is load-bearing; a build that emitted "created" twice and
    // "updated" never would still have to fail here.
    //
    // Making delivery ordered instead would be a behaviour change, and worth it
    // only alongside a documented promise that consumers may rely on the order.
    // Nothing currently makes one.
    kinds.sort();
    assert_eq!(kinds, vec!["created", "deleted", "updated"]);
    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

#[test]
fn a_delete_still_reports_the_category_it_had() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, rx) = capture(2);
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &url);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed", "engineering");
    let _created = received(&rx);
    remind_me_core::db::queries::delete_memory(&conn, &id).unwrap();
    events::drain();

    // A hard delete removes the row, so the category has to be captured before
    // the delete or the event has nothing left to read and would guess.
    let deleted = received(&rx);
    assert_eq!(deleted["event"], "deleted");
    assert_eq!(deleted["category"], "engineering");

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

// ---------------------------------------------------------------------------
// Silence where silence is required
// ---------------------------------------------------------------------------

#[test]
fn an_unconfigured_stream_is_a_true_no_op() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);

    assert!(!events::enabled());

    // No thread is started at all, rather than one that discovers it has
    // nowhere to go — this runs on every single write.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "no consumer configured", "general");
    events::drain();
}

#[test]
fn a_blank_url_counts_as_unconfigured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, "   ");

    // A blank env var is how "unset" arrives from a lot of process managers.
    assert!(!events::enabled());

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

#[test]
fn a_sync_applied_write_emits_nothing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, rx) = capture(1);
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &url);
    crate::test_env::set_var(remind_me_core::sync::NODE_ID_ENV, "node-events-test");
    crate::test_env::set_var(remind_me_core::sync::HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(remind_me_core::sync::SYNC_SECRET_ENV, "shh");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let record = remind_me_core::sync::SyncRecord {
        id: "mem_from_peer".into(),
        content: "arrived over sync".into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        created_at: "2030-01-01T00:00:00+00:00".into(),
        updated_at: "2030-01-01T00:00:00+00:00".into(),
        capture_id: None,
        node_id: Some("peer".into()),
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
    remind_me_core::sync::upsert_record(&conn, &record).unwrap();
    events::drain();

    // Emitting on a sync-applied write is how two synced nodes would echo each
    // other's mutations back and forth forever.
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "a record arriving from a peer must not emit a local event"
    );

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
    crate::test_env::remove_var(remind_me_core::sync::NODE_ID_ENV);
    crate::test_env::remove_var(remind_me_core::sync::HUB_URL_ENV);
    crate::test_env::remove_var(remind_me_core::sync::SYNC_SECRET_ENV);
}

// ---------------------------------------------------------------------------
// Failure containment
// ---------------------------------------------------------------------------

#[test]
fn a_dead_endpoint_does_not_fail_the_write() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Bound then dropped, so the port is almost certainly refusing.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/events", listener.local_addr().unwrap())
    };
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &dead);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // A write is the user's data; a webhook is someone's convenience, and the
    // second must never be able to cost the first.
    let id = add(&conn, "the write must survive", "general");
    events::drain();

    assert!(remind_me_core::db::queries::get_memory_by_id(&conn, &id)
        .unwrap()
        .is_some());

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

#[test]
fn there_is_no_throttle() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, rx) = capture(5);
    crate::test_env::set_var(events::EVENT_WEBHOOK_URL_ENV, &url);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..5 {
        add(&conn, &format!("memory {i}"), "general");
    }
    events::drain();

    // Unlike a human-facing notification, suppressing a "repeat" here would
    // silently drop a real mutation the consumer needs. Completeness is the
    // whole point of a separate stream.
    for _ in 0..5 {
        assert_eq!(received(&rx)["event"], "created");
    }

    crate::test_env::remove_var(events::EVENT_WEBHOOK_URL_ENV);
}

#[test]
fn the_payload_builder_matches_what_is_sent() {
    let built = events::payload(Event::Updated, "mem_x", "engineering");

    assert_eq!(built["event"], "updated");
    assert_eq!(built["memory_id"], "mem_x");
    assert_eq!(built["category"], "engineering");
    // Exactly four keys: a fifth added carelessly is how content leaks in.
    assert_eq!(built.as_object().unwrap().len(), 4, "{built}");
}
