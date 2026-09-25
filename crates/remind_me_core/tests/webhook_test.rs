//! Coverage for push ingestion over HTTP.
//!
//! Most of these drive [`webhook::serve_once`] over an in-memory stream rather
//! than a socket. That is not a shortcut around the network: the protocol
//! handling, the auth check and the size caps are all in `serve_once`, and
//! exercising them through a real listener would add scheduling flake to every
//! assertion for nothing. Two tests do go over TCP, to cover the parts a fake
//! stream cannot: that a port is really bound, and that stopping really joins.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{CLIENT_ENV, NODE_ID_ENV};
use remind_me_core::webhook::{
    self, constant_time_eq, validate_payload, Webhook, WebhookConfig, WebhookCounters,
    MAX_BODY_BYTES, MAX_HEAD_BYTES,
};
use remind_me_core::Database;
use rusqlite::Connection;
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};

const SECRET: &str = "s3cret-token";

/// A `Read + Write` pair standing in for an accepted connection.
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

fn config() -> WebhookConfig {
    WebhookConfig::new("127.0.0.1", 0, SECRET).expect("a non-empty secret builds a config")
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

fn authed(body: &str) -> String {
    request("POST", "/ingest", Some(&format!("Bearer {}", SECRET)), body)
}

/// Run one request against a fresh store, returning (status, parsed body).
fn serve(conn: &Connection, raw: &str) -> (u16, serde_json::Value) {
    let counters = WebhookCounters::default();
    serve_with(conn, raw, &counters)
}

/// Turn the rate limiter off for this binary.
///
/// These tests assert ingest *semantics*, and the limiter (issue #121) is a
/// process-wide singleton bucketing by peer address — so with it on, the
/// 61st request anywhere in this binary starts 429ing and every test becomes
/// coupled to how many requests every other one happens to make. Rate
/// limiting on this endpoint has its own binary, `webhook_rate_limit_test.rs`,
/// where it is the subject rather than ambient interference.
///
/// Set from every helper rather than once, because the tests run in parallel
/// and there is no ordered setup hook.
fn disable_rate_limit() {
    crate::test_env::set_var(remind_me_core::rate_limit::RATE_LIMIT_ENABLED_ENV, "");
}

fn serve_with(
    conn: &Connection,
    raw: &str,
    counters: &WebhookCounters,
) -> (u16, serde_json::Value) {
    disable_rate_limit();
    let mut stream = FakeStream::new(raw.as_bytes().to_vec());
    webhook::serve_once(&mut stream, &config(), conn, counters).expect("no I/O failure");
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

const CHAT: &str = r#"[{"role":"user","content":"what is a quokka"},{"role":"assistant","content":"a small marsupial found on Rottnest Island"}]"#;

fn push_body(filename: &str, content: &str) -> String {
    serde_json::json!({ "filename": filename, "content": content }).to_string()
}

// ---------------------------------------------------------------------------
// Configuration: off unless a secret is set
// ---------------------------------------------------------------------------

#[test]
fn an_empty_secret_cannot_produce_a_configuration() {
    // The type has no representation for an unauthenticated endpoint, so
    // there is no path from an empty secret to a bound port.
    assert!(WebhookConfig::new("127.0.0.1", 8769, "").is_none());
}

#[test]
fn a_disabled_webhook_says_what_to_set_rather_than_reporting_a_bare_false() {
    let status = Webhook::Disabled.status();

    assert!(!status.enabled);
    assert!(!status.running);
    assert!(status.bind.is_none());
    // The distinguishing field: nothing failed, nobody configured one.
    assert!(status.start_error.is_none());
    let hint = status.hint.expect("a disabled endpoint explains itself");
    assert!(
        hint.contains(webhook::WEBHOOK_SECRET_ENV),
        "the hint names the variable, got {hint}"
    );
}

#[test]
fn a_failure_to_bind_is_distinguishable_from_being_disabled() {
    let db = Arc::new(Database::open_in_memory().unwrap());

    // Hold the port first, so the webhook's bind is the one that loses.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();

    let webhook = Webhook::start(WebhookConfig::new("127.0.0.1", port, SECRET).unwrap(), db);
    let status = webhook.status();

    // Both are "not running", and that is exactly why a bare boolean will not
    // do: this one was asked for and could not start.
    assert!(status.enabled);
    assert!(!status.running);
    assert_eq!(status.port, Some(port));
    assert!(
        status.start_error.is_some(),
        "a bind failure reports why, got {status:?}"
    );
}

// ---------------------------------------------------------------------------
// The token comparison
// ---------------------------------------------------------------------------

#[test]
fn constant_time_comparison_still_compares() {
    // Timing is what it is for; being correct is table stakes.
    assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
    assert!(!constant_time_eq(b"Bearer abc", b"Bearer abd"));
    assert!(!constant_time_eq(b"Bearer abc", b"Bearer ab"));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(constant_time_eq(b"", b""));
    // A prefix must not pass: the accumulator folds in the length difference
    // rather than stopping at the shorter of the two.
    assert!(!constant_time_eq(b"Bearer", b"Bearer abc"));
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[test]
fn a_push_without_a_token_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let (status, body) = serve(
        &db.conn(),
        &request("POST", "/ingest", None, &push_body("chat.json", CHAT)),
    );

    assert_eq!(status, 401);
    assert_eq!(body["error"], "unauthorized");
    assert_eq!(stored(&db.conn()), 0, "nothing is written on a refusal");
}

#[test]
fn a_push_with_the_wrong_token_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let raw = request(
        "POST",
        "/ingest",
        Some("Bearer not-the-secret"),
        &push_body("chat.json", CHAT),
    );

    let (status, _) = serve(&db.conn(), &raw);

    assert_eq!(status, 401);
    assert_eq!(stored(&db.conn()), 0);
}

#[test]
fn a_token_missing_the_bearer_prefix_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let raw = request(
        "POST",
        "/ingest",
        Some(SECRET),
        &push_body("chat.json", CHAT),
    );

    assert_eq!(serve(&db.conn(), &raw).0, 401);
}

#[test]
fn an_unauthenticated_caller_cannot_map_the_routes() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // Auth is checked before routing, so a wrong path and a right one look
    // identical to someone without the token. If routing came first, the
    // difference between 404 and 401 would enumerate the endpoint.
    let ingest = serve(&conn, &request("POST", "/ingest", None, "{}"));
    let elsewhere = serve(&conn, &request("POST", "/admin", None, "{}"));

    assert_eq!(ingest, elsewhere);
    assert_eq!(ingest.0, 401);
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn an_authenticated_request_to_another_path_is_not_found() {
    let db = Database::open_in_memory().unwrap();
    let raw = request(
        "POST",
        "/somewhere-else",
        Some(&format!("Bearer {}", SECRET)),
        "{}",
    );

    assert_eq!(serve(&db.conn(), &raw).0, 404);
}

#[test]
fn a_get_is_not_found_and_a_put_is_not_allowed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let bearer = format!("Bearer {}", SECRET);

    assert_eq!(
        serve(&conn, &request("GET", "/ingest", Some(&bearer), "")).0,
        404
    );
    assert_eq!(
        serve(&conn, &request("PUT", "/ingest", Some(&bearer), "{}")).0,
        405
    );
}

#[test]
fn a_query_string_does_not_change_the_route() {
    let db = Database::open_in_memory().unwrap();
    let raw = request(
        "POST",
        "/ingest?from=ci",
        Some(&format!("Bearer {}", SECRET)),
        &push_body("chat.json", CHAT),
    );

    assert_eq!(serve(&db.conn(), &raw).0, 200);
}

// ---------------------------------------------------------------------------
// Size caps
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_body_is_refused_without_being_read() {
    let db = Database::open_in_memory().unwrap();
    // Declares far more than the cap while sending almost nothing: the
    // refusal has to come from the declared length, before any buffering,
    // or the cap would be no protection at all.
    let raw = format!(
        "POST /ingest HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\nxx",
        SECRET,
        MAX_BODY_BYTES + 1
    );

    let (status, body) = serve(&db.conn(), &raw);

    assert_eq!(status, 413);
    assert_eq!(body["error"], "request body too large");
}

#[test]
fn a_body_exactly_at_the_cap_is_not_refused_for_size() {
    let db = Database::open_in_memory().unwrap();
    // At the boundary the size check passes and the request fails later, on
    // its contents — which is what proves the cap is `>` and not `>=`.
    let raw = format!(
        "POST /ingest HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{{}}",
        SECRET, MAX_BODY_BYTES
    );

    let (status, body) = serve(&db.conn(), &raw);

    assert_eq!(status, 400, "truncated, not oversized: {body}");
}

#[test]
fn an_oversized_header_block_is_refused() {
    let db = Database::open_in_memory().unwrap();
    // No terminator at all, so the only thing that can stop this is the cap on
    // what has been buffered.
    let raw = format!(
        "POST /ingest HTTP/1.1\r\nX-Filler: {}\r\n",
        "a".repeat(MAX_HEAD_BYTES + 1024)
    );

    let (status, _) = serve(&db.conn(), &raw);

    assert_eq!(status, 431);
}

#[test]
fn a_missing_or_unparseable_content_length_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let bearer = format!("Bearer {}", SECRET);

    let none = format!("POST /ingest HTTP/1.1\r\nAuthorization: {}\r\n\r\n", bearer);
    let (status, body) = serve(&conn, &none);
    assert_eq!(status, 400);
    assert_eq!(body["error"], "missing request body");

    let junk = format!(
        "POST /ingest HTTP/1.1\r\nAuthorization: {}\r\nContent-Length: banana\r\n\r\n",
        bearer
    );
    let (status, body) = serve(&conn, &junk);
    assert_eq!(status, 400);
    // Distinguished from an absent header: one is a client that sent no body,
    // the other is a client sending something malformed.
    assert_eq!(body["error"], "invalid content-length");

    let empty = format!(
        "POST /ingest HTTP/1.1\r\nAuthorization: {}\r\nContent-Length: 0\r\n\r\n",
        bearer
    );
    assert_eq!(serve(&conn, &empty).0, 400);
}

// ---------------------------------------------------------------------------
// Payload validation
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let (status, body) = serve(&db.conn(), &authed("{not json"));

    assert_eq!(status, 400);
    assert_eq!(body["error"], "malformed JSON");
}

#[test]
fn every_field_is_checked_before_the_import_pipeline_sees_it() {
    let cases: [(serde_json::Value, &str); 9] = [
        (serde_json::json!([]), "invalid ingest payload"),
        (
            serde_json::json!({ "content": "x" }),
            "missing or invalid 'filename'",
        ),
        (
            serde_json::json!({ "filename": "   ", "content": "x" }),
            "missing or invalid 'filename'",
        ),
        (
            serde_json::json!({ "filename": "a.md" }),
            "missing or invalid 'content' (must be a UTF-8 text string)",
        ),
        (
            serde_json::json!({ "filename": "a.md", "content": "x", "kind": "sideways" }),
            "invalid kind: \"sideways\" (use 'auto', 'chat', or 'document')",
        ),
        (
            serde_json::json!({ "filename": "a.md", "content": "x", "category": 7 }),
            "'category' must be a string",
        ),
        (
            serde_json::json!({ "filename": "a.md", "content": "x", "tags": ["ok", 3] }),
            "'tags' must be a list of strings",
        ),
        (
            serde_json::json!({ "filename": "a.md", "content": "x", "extract_mode": [] }),
            "'extract_mode' must be a string",
        ),
        (
            serde_json::json!({ "filename": "a.md", "content": "x", "max_length": 5 }),
            "'max_length' must be an integer between 100 and 50000",
        ),
    ];

    for (payload, expected) in cases {
        let error = validate_payload(&payload).err();
        assert_eq!(error.as_deref(), Some(expected), "for payload {payload}");
    }
}

#[test]
fn a_boolean_max_length_does_not_coerce_to_a_number() {
    // `true` is 1 in enough languages that a lenient parse would accept it and
    // then chunk at one character.
    let payload = serde_json::json!({ "filename": "a.md", "content": "x", "max_length": true });

    assert!(validate_payload(&payload).is_err());
}

#[test]
fn the_defaults_match_a_file_import() {
    let request = validate_payload(&serde_json::json!({
        "filename": "chat.json", "content": CHAT
    }))
    .expect("filename and content are the only required fields");

    assert_eq!(request.category, "chat_import");
    assert_eq!(request.extract_mode, "assistant_messages");
    assert_eq!(request.max_length, 10_000);
    assert!(request.tags.is_empty());
}

// ---------------------------------------------------------------------------
// Ingestion
// ---------------------------------------------------------------------------

fn stored(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_valid_push_becomes_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let (status, body) = serve(&conn, &authed(&push_body("chat.json", CHAT)));

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "imported");
    let content: String = conn
        .query_row("SELECT content FROM memories", [], |r| r.get(0))
        .unwrap();
    assert!(
        content.contains("marsupial"),
        "the assistant message landed, got {content}"
    );
}

/// `REMIND_ME_NODE_ID`/`REMIND_ME_CLIENT` are process-global; no other test
/// in this file touches them, so this is the only one that needs the guard.
static PROVENANCE_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_pushed_memory_is_stamped_with_the_configured_node_and_client() {
    // #266: `import_content` -- the shared writer behind `import_chat`,
    // `import_directory`, and this webhook push -- never stamped `node_id`/
    // `client` at all, reopening the bug #258 fixed for six other write
    // paths. This is the webhook-push fixture #266 itself asked for.
    let _guard = PROVENANCE_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(NODE_ID_ENV, "webhook-test-node");
    crate::test_env::set_var(CLIENT_ENV, "webhook-test-client");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (status, body) = serve(&conn, &authed(&push_body("chat.json", CHAT)));
    assert_eq!(status, 200, "{body}");

    let (node_id, client): (Option<String>, String) = conn
        .query_row("SELECT node_id, client FROM memories LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(CLIENT_ENV);

    assert_eq!(node_id.as_deref(), Some("webhook-test-node"));
    assert_eq!(client, "webhook-test-client");
}

#[test]
fn a_pushed_memory_keeps_the_source_a_file_import_would_have_given_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    serve(&conn, &authed(&push_body("chat.json", CHAT)));

    let source: String = conn
        .query_row("SELECT source FROM memories", [], |r| r.get(0))
        .unwrap();
    // Not "webhook". `source` feeds dedup, the normalize_batch selection and
    // the vitality source prior, and a database is meant to be readable by
    // `remind_me`, which stores pushed content under exactly these values. The
    // arrival channel is recorded in metadata instead, where it costs nothing.
    assert_eq!(source, "chat_import");

    let metadata: String = conn
        .query_row("SELECT metadata FROM memories", [], |r| r.get(0))
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["ingest"], "webhook");
    assert_eq!(metadata["filename"], "chat.json");
}

#[test]
fn a_pushed_document_is_chunked_as_a_document() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let doc = "# Rottnest\n\nA small island.\n\n# Quokkas\n\nThey live there.\n";

    let (status, _) = serve(&conn, &authed(&push_body("notes.md", doc)));

    assert_eq!(status, 200);
    let source: String = conn
        .query_row("SELECT source FROM memories LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(source, "document_import");
    assert!(stored(&conn) >= 2, "one memory per section");
}

#[test]
fn pushing_the_same_content_twice_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let counters = WebhookCounters::default();

    let (first, _) = serve_with(&conn, &authed(&push_body("chat.json", CHAT)), &counters);
    let after_first = stored(&conn);
    // A different display name, byte-identical content: dedup is on the
    // content hash, so the rename must not smuggle a second copy in.
    let (second, body) = serve_with(&conn, &authed(&push_body("renamed.json", CHAT)), &counters);

    assert_eq!(first, 200);
    assert_eq!(second, 200);
    assert_eq!(body["status"], "skipped");
    assert_eq!(body["reason"], "already_imported");
    assert_eq!(stored(&conn), after_first);
}

#[test]
fn an_unsupported_format_is_refused_as_content_not_as_a_bad_request() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // A pushed filename names nothing on disk, so it gets held to the same
    // format rule a real file would be — otherwise the extension would be a
    // way to reach a parser the file importer will not reach.
    //
    // `.exe` rather than `.pdf`: this used to use a PDF, and #153 made that a
    // supported format, which quietly turned this into a test of the PDF
    // parser instead of the format gate. The extension only has to be one
    // nothing will ever import.
    let (status, body) = serve(&conn, &authed(&push_body("payload.exe", "MZ")));

    // 422, not 400: the request was well-formed, the content was not usable.
    assert_eq!(status, 422);
    assert_eq!(body["status"], "failed");
    assert!(
        body["reason"].as_str().unwrap().contains("unsupported"),
        "got {body}"
    );
    assert_eq!(stored(&conn), 0);
}

#[test]
fn a_document_import_of_a_chat_export_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let body = serde_json::json!({
        "filename": "chat.json", "content": CHAT, "kind": "document"
    })
    .to_string();

    let (status, body) = serve(&conn, &authed(&body));

    assert_eq!(status, 422);
    assert!(body["reason"]
        .as_str()
        .unwrap()
        .contains("document import does not support"));
}

#[test]
fn the_counters_tally_each_outcome_separately() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let counters = WebhookCounters::default();

    serve_with(&conn, &authed(&push_body("chat.json", CHAT)), &counters);
    serve_with(&conn, &authed(&push_body("again.json", CHAT)), &counters);
    // See the note in the unsupported-format test above for why this is not
    // a `.pdf` any more.
    serve_with(&conn, &authed(&push_body("bad.exe", "x")), &counters);
    // A rejection that never reaches the importer is not an ingestion
    // outcome, so it moves none of these.
    serve_with(&conn, &request("POST", "/ingest", None, "{}"), &counters);

    let (ingested, skipped, errored, errors) = counters.snapshot();

    assert_eq!((ingested, skipped, errored), (1, 1, 1));
    assert_eq!(errors.len(), 1, "the reason is kept, not just the count");
    assert!(errors[0].contains("unsupported"), "got {errors:?}");
}

#[test]
fn only_the_most_recent_errors_are_kept() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let counters = WebhookCounters::default();

    // Unbounded error history on a network-facing endpoint is a way for a
    // hostile client to grow this process's memory one bad push at a time.
    for i in 0..40 {
        serve_with(
            &conn,
            &authed(&push_body(&format!("bad{i}.pdf"), "x")),
            &counters,
        );
    }

    let (_, _, errored, errors) = counters.snapshot();
    assert_eq!(errored, 40, "the count is complete");
    assert!(
        errors.len() <= 10,
        "the history is not, got {}",
        errors.len()
    );
}

// ---------------------------------------------------------------------------
// Over a real socket
// ---------------------------------------------------------------------------

fn post(port: u16, auth: &str, body: &str) -> (u16, serde_json::Value) {
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the port is listening");
    stream
        .write_all(
            format!(
                "POST /ingest HTTP/1.1\r\nHost: localhost\r\nAuthorization: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                auth,
                body.len(),
                body
            )
            .as_bytes(),
        )
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    parse_response(&raw)
}

#[test]
fn a_running_endpoint_accepts_a_push_over_the_network() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    let webhook = Webhook::start(
        WebhookConfig::new("127.0.0.1", 0, SECRET).unwrap(),
        Arc::clone(&db),
    );
    let status = webhook.status();
    assert!(status.running, "{status:?}");
    let port = status.port.unwrap();
    assert_ne!(port, 0, "port 0 resolves to a real bound port");

    let (code, body) = post(
        port,
        &format!("Bearer {}", SECRET),
        &push_body("chat.json", CHAT),
    );
    assert_eq!(code, 200, "{body}");

    let (refused, _) = post(port, "Bearer wrong", &push_body("other.json", CHAT));
    assert_eq!(refused, 401);

    let after = webhook.status();
    assert_eq!(after.requests_ingested, 1);
    assert_eq!(stored(&db.conn()), 1);
}

#[test]
fn stopping_joins_the_serving_thread_before_the_database_could_close() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    let mut webhook = Webhook::start(
        WebhookConfig::new("127.0.0.1", 0, SECRET).unwrap(),
        Arc::clone(&db),
    );
    let port = webhook.status().port.unwrap();
    assert_eq!(
        Arc::strong_count(&db),
        2,
        "the serving thread holds its own handle on the database"
    );

    webhook.stop();

    // This is the SE-07 property, stated as something checkable rather than as
    // a call-order convention: once stop() returns, no other holder of the
    // database is left alive, so closing the connections cannot race a
    // handler mid-write.
    assert_eq!(Arc::strong_count(&db), 1);
    assert!(!webhook.status().running);
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the listener is released, not just the loop"
    );

    webhook.stop(); // idempotent
}
