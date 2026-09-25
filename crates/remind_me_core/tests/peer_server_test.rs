//! Protocol-level coverage for the sync peer server's `serve_once`, over an
//! in-memory stream rather than a socket — the same reasoning
//! `webhook_test.rs` gives for doing the same: the auth check, routing, and
//! size caps all live in `serve_once`, and a real listener would only add
//! scheduling flake. The real-network push/pull round trip against a real
//! `TcpListener` is covered separately in `sync_test.rs`.
//!
//! `REMIND_ME_SYNC_SECRET`/`REMIND_ME_NODE_ID`/`REMIND_ME_HUB_URL` are
//! process-global; the two tests here that touch them hold `ENV_LOCK`.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::sync::{
    self, serve_once, PeerServerConfig, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::io::{Cursor, Read, Write};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const SECRET: &str = "s3cret-token";

struct FakeStream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl FakeStream {
    fn new(request: impl Into<Vec<u8>>) -> Self {
        Self {
            input: Cursor::new(request.into()),
            output: Vec::new(),
        }
    }
}

impl Read for FakeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for FakeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn config() -> PeerServerConfig {
    PeerServerConfig::new("127.0.0.1", 0, SECRET, "hub-node")
}

fn request(method: &str, path: &str, auth: Option<&str>, body: &str) -> String {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: localhost\r\n", method, path);
    if let Some(auth) = auth {
        head.push_str(&format!("Authorization: {}\r\n", auth));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    head.push_str(body);
    head
}

fn authed(method: &str, path: &str, body: &str) -> String {
    request(method, path, Some(&format!("Bearer {}", SECRET)), body)
}

fn serve(conn: &Connection, raw: &str) -> (u16, serde_json::Value) {
    let mut stream = FakeStream::new(raw.as_bytes().to_vec());
    serve_once(&mut stream, &config(), conn).expect("no I/O failure");
    parse_response(&stream.output)
}

fn parse_response(raw: &[u8]) -> (u16, serde_json::Value) {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a response has a header block");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, serde_json::from_str(body).unwrap_or_default())
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

// ---------------------------------------------------------------------------
// Auth comes before routing
// ---------------------------------------------------------------------------

#[test]
fn a_request_without_a_bearer_token_is_unauthorized_even_for_health() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(&db.conn(), &request("GET", "/health", None, ""));
    assert_eq!(status, 401);
}

#[test]
fn the_wrong_token_is_unauthorized() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(
        &db.conn(),
        &request("GET", "/health", Some("Bearer wrong"), ""),
    );
    assert_eq!(status, 401);
}

#[test]
fn an_unauthenticated_caller_cannot_distinguish_a_real_path_from_a_fake_one() {
    let db = Database::open_in_memory().unwrap();
    let (real, _) = serve(&db.conn(), &request("GET", "/sync/pull", None, ""));
    let (fake, _) = serve(&db.conn(), &request("GET", "/nonexistent", None, ""));
    assert_eq!(real, 401);
    assert_eq!(fake, 401);
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn health_reports_ok_and_this_nodes_id() {
    let db = Database::open_in_memory().unwrap();
    let (status, body) = serve(&db.conn(), &authed("GET", "/health", ""));
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["node_id"], "hub-node");
    assert!(body["time"].is_string());
}

#[test]
fn an_unknown_path_is_not_found() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(&db.conn(), &authed("GET", "/nonexistent", ""));
    assert_eq!(status, 404);
}

#[test]
fn a_get_on_the_push_path_is_not_allowed() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(&db.conn(), &authed("GET", "/sync/push", ""));
    assert_eq!(status, 405);
}

#[test]
fn a_post_on_the_health_path_is_not_allowed() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(&db.conn(), &authed("POST", "/health", ""));
    assert_eq!(status, 405);
}

// ---------------------------------------------------------------------------
// /sync/push
// ---------------------------------------------------------------------------

#[test]
fn push_with_malformed_json_is_a_bad_request() {
    let db = Database::open_in_memory().unwrap();
    let (status, _) = serve(&db.conn(), &authed("POST", "/sync/push", "not json"));
    assert_eq!(status, 400);
}

#[test]
fn push_without_a_records_array_is_a_bad_request() {
    let db = Database::open_in_memory().unwrap();
    let body = serde_json::json!({ "node_id": "peer-1" }).to_string();
    let (status, response) = serve(&db.conn(), &authed("POST", "/sync/push", &body));
    assert_eq!(status, 400);
    assert!(response["error"].as_str().unwrap().contains("records"));
}

#[test]
fn push_applies_valid_records_and_reports_processed_ids() {
    let db = Database::open_in_memory().unwrap();
    let body = serde_json::json!({
        "node_id": "peer-1",
        "records": [{
            "id": "mem_pushed",
            "content": "pushed content",
            "created_at": "2026-01-01T00:00:00+00:00",
            "updated_at": "2026-01-01T00:00:00+00:00",
        }],
    })
    .to_string();

    let (status, response) = serve(&db.conn(), &authed("POST", "/sync/push", &body));

    assert_eq!(status, 200);
    assert_eq!(response["accepted"], 1);
    assert_eq!(response["processed_ids"], serde_json::json!(["mem_pushed"]));
    assert_eq!(response["failed"], 0);
    let stored: String = db
        .conn()
        .query_row(
            "SELECT content FROM memories WHERE id = 'mem_pushed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "pushed content");
}

#[test]
fn push_counts_a_malformed_record_as_failed_without_losing_the_good_ones() {
    let db = Database::open_in_memory().unwrap();
    let body = serde_json::json!({
        "node_id": "peer-1",
        "records": [
            {"id": "mem_good", "content": "ok", "created_at": "2026-01-01T00:00:00+00:00", "updated_at": "2026-01-01T00:00:00+00:00"},
            {"id": "mem_bad_no_content"},
        ],
    })
    .to_string();

    let (status, response) = serve(&db.conn(), &authed("POST", "/sync/push", &body));

    assert_eq!(status, 200);
    assert_eq!(response["accepted"], 1);
    assert_eq!(response["failed"], 1);
    assert_eq!(response["processed_ids"], serde_json::json!(["mem_good"]));
}

// ---------------------------------------------------------------------------
// /sync/pull
// ---------------------------------------------------------------------------

#[test]
fn pull_with_no_query_returns_everything_oldest_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "first");
    add(&conn, "second");

    let (status, response) = serve(&conn, &authed("GET", "/sync/pull", ""));

    assert_eq!(status, 200);
    let records = response["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(response["count"], 2);
    assert_eq!(records[0]["content"], "first");
    assert_eq!(records[1]["content"], "second");
}

#[test]
fn pull_tags_and_metadata_are_real_json_not_double_encoded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "content");
    conn.execute(
        "UPDATE memories SET tags = '[\"a\",\"b\"]', metadata = '{\"k\":\"v\"}' WHERE id = ?",
        [&id],
    )
    .unwrap();

    let (_, response) = serve(&conn, &authed("GET", "/sync/pull", ""));

    let record = &response["records"][0];
    assert_eq!(record["tags"], serde_json::json!(["a", "b"]));
    assert_eq!(record["metadata"], serde_json::json!({"k": "v"}));
}

#[test]
fn pull_excludes_the_callers_own_node_id() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mine = add(&conn, "mine");
    let theirs = add(&conn, "theirs");
    conn.execute(
        "UPDATE memories SET node_id = 'caller-node' WHERE id = ?",
        [&mine],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET node_id = 'someone-else' WHERE id = ?",
        [&theirs],
    )
    .unwrap();

    let (_, response) = serve(
        &conn,
        &authed("GET", "/sync/pull?exclude_node=caller-node", ""),
    );

    let records = response["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"], "theirs");
}

#[test]
fn pull_since_excludes_everything_at_or_before_the_cursor() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "old");
    conn.execute(
        "UPDATE memories SET updated_at = '2020-01-01T00:00:00+00:00'",
        [],
    )
    .unwrap();
    add(&conn, "new");

    let (_, response) = serve(
        &conn,
        &authed("GET", "/sync/pull?since=2025-01-01T00:00:00+00:00", ""),
    );

    let records = response["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"], "new");
}

#[test]
fn pull_limit_is_clamped_to_the_server_side_maximum() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "one");

    // A client-requested limit far above MAX_PULL_LIMIT must not be honored
    // literally -- it is clamped, not trusted.
    let (status, response) = serve(&conn, &authed("GET", "/sync/pull?limit=999999", ""));

    assert_eq!(status, 200);
    assert_eq!(response["records"].as_array().unwrap().len(), 1);
}

#[test]
fn pull_over_an_empty_store_returns_no_records() {
    let db = Database::open_in_memory().unwrap();
    let (status, response) = serve(&db.conn(), &authed("GET", "/sync/pull", ""));
    assert_eq!(status, 200);
    assert_eq!(response["records"].as_array().unwrap().len(), 0);
    assert_eq!(response["count"], 0);
}

// ---------------------------------------------------------------------------
// Configuration gating
// ---------------------------------------------------------------------------

#[test]
fn peer_server_config_is_none_without_a_secret() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(SYNC_SECRET_ENV);
    assert!(PeerServerConfig::from_env().is_none());
}

#[test]
fn sync_enabled_requires_all_three_of_node_id_hub_url_and_secret() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
    assert!(!sync::sync_enabled());

    crate::test_env::set_var(NODE_ID_ENV, "node-a");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub:8766");
    assert!(!sync::sync_enabled(), "still missing the secret");

    crate::test_env::set_var(SYNC_SECRET_ENV, "s3cret");
    assert!(sync::sync_enabled());

    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
}

// ---------------------------------------------------------------------------
// /count (gap A5, issue #113)
// ---------------------------------------------------------------------------

#[test]
fn count_reports_the_hubs_field_shape() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "one");
    add(&conn, "two");

    let (status, body) = serve(&conn, &authed("GET", "/count", ""));

    assert_eq!(status, 200);
    // Field-for-field the hub's shape, so one client-side comparator serves
    // both remotes. A peer-shaped variant would mean a second copy of the diff
    // logic — which is how the two eventually disagree about what drift means.
    assert_eq!(body["role"], "peer");
    assert_eq!(body["node_id"], "hub-node");
    assert_eq!(body["memories"]["total"], 2);
    assert_eq!(body["memories"]["live"], 2);
    assert_eq!(body["memories"]["tombstones"], 0);
    assert_eq!(body["entities"], 0);
    assert_eq!(body["memory_entities"], 0);
    assert_eq!(body["entity_relations"], 0);
    assert!(body["version"].is_string());
    assert!(body["time"].is_string());
}

#[test]
fn count_always_reports_approximate_false_and_never_omits_it() {
    let db = Database::open_in_memory().unwrap();

    let (_, body) = serve(&db.conn(), &authed("GET", "/count", ""));

    // A peer has no planner estimates to offer — the hub's `?approx=1` is a
    // Postgres reltuples read with no SQLite equivalent. The field is still
    // present, because a caller should not have to know which kind of remote
    // it is talking to in order to read the answer.
    assert_eq!(body["approximate"], false);
    assert!(
        body.get("approximate").is_some(),
        "present-and-false, not omitted"
    );
}

#[test]
fn count_includes_tombstones_in_the_total() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "will be tombstoned");
    add(&conn, "still live");
    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-01T00:00:00+00:00' WHERE id = ?",
        [&id],
    )
    .unwrap();

    let (_, body) = serve(&conn, &authed("GET", "/count", ""));

    // Deliberately NOT filtered on deleted_at. Both ends of a reconcile have
    // to count identically: the hub counts every row and reports tombstones
    // separately, so filtering here would make a healthy peer look permanently
    // behind by its own tombstone count.
    assert_eq!(body["memories"]["total"], 2);
    assert_eq!(body["memories"]["live"], 1);
    assert_eq!(body["memories"]["tombstones"], 1);
}

#[test]
fn count_requires_authorization() {
    let db = Database::open_in_memory().unwrap();

    let (status, _) = serve(&db.conn(), &request("GET", "/count", None, ""));

    assert_eq!(status, 401);
}

#[test]
fn a_post_on_the_count_path_is_not_allowed() {
    let db = Database::open_in_memory().unwrap();

    // 405 rather than 404: the path exists, the method does not. Registering
    // it in the known-paths list is what makes that distinction possible.
    let (status, _) = serve(&db.conn(), &authed("POST", "/count", ""));

    assert_eq!(status, 405);
}

#[test]
fn health_also_reports_the_serving_build() {
    let db = Database::open_in_memory().unwrap();

    let (_, body) = serve(&db.conn(), &authed("GET", "/health", ""));

    // A reconcile reports which build each side is running, and this is where
    // the other side reads it from.
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}
