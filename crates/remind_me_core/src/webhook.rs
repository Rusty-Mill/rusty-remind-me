//! Push ingestion over HTTP: content arrives from the network instead of a
//! file, and becomes memories through the same importer.
//!
//! The folder watcher covers the case where a sender can drop a file where
//! this process can see it. A CI job, a chat-export tool, or an automation on
//! another machine usually cannot, and staging a file just to be noticed is a
//! detour through the filesystem that buys nothing. This is the direct route:
//! `POST /ingest`, JSON body, same parsing, same dedup, same chunking.
//!
//! # Off unless a secret is configured
//!
//! With no [`WEBHOOK_SECRET_ENV`] set, nothing binds a port. This is the
//! behaviour, not a deployment recommendation — an endpoint that writes
//! arbitrary content into memory with no authentication is worse than no
//! endpoint at all, so there is no way to ask for one.
//!
//! The bind address defaults to localhost for the same reason. Widening it is
//! a deliberate act.
//!
//! # Three properties that are load-bearing, not decorative
//!
//! **The token comparison is constant-time.** A byte-by-byte `==` returns
//! faster on a wrong first byte than a wrong last byte, which is enough to
//! recover a secret one byte at a time over enough requests. See
//! [`constant_time_eq`].
//!
//! **The request body is capped** at [`MAX_BODY_BYTES`], and the header block
//! at [`MAX_HEAD_BYTES`], both *before* anything is buffered. A declared
//! `Content-Length` is never trusted as an allocation size.
//!
//! **The listener stops before the database connections close.** See
//! [`Webhook`] for how that ordering is made structural rather than
//! remembered.
//!
//! # One connection at a time, deliberately
//!
//! Connections are accepted and served serially rather than one thread per
//! connection. Every request takes the database lock to import, so handling
//! them concurrently would not make them finish sooner — it would only move
//! the queue from the kernel's accept backlog into unbounded threads inside
//! this process. Reads and writes carry a timeout so a stalled client releases
//! the loop instead of holding it.
//!
//! # Wire format
//!
//! ```text
//! POST /ingest HTTP/1.1
//! Authorization: Bearer <secret>
//! Content-Length: <n>
//!
//! {"filename": "chat.json", "content": "<utf-8 text>",
//!  "category": "chat_import", "tags": [], "extract_mode": "assistant_messages",
//!  "max_length": 10000, "kind": "auto"}
//! ```
//!
//! Only `filename` and `content` are required; the rest default exactly as
//! `remind_me_import_chat` does. `content` is UTF-8 text — this endpoint
//! ingests the same text-native formats the file importer does, not arbitrary
//! binary.

use crate::importer::import_bytes;
use crate::models::{ImportKind, ImportOutcome};
use crate::Database;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Bearer token required on every request. Unset means the endpoint does not
/// exist.
pub const WEBHOOK_SECRET_ENV: &str = "REMIND_ME_WEBHOOK_SECRET";
/// Bind address. Defaults to localhost; widen deliberately.
pub const WEBHOOK_BIND_ENV: &str = "REMIND_ME_WEBHOOK_BIND";
pub const WEBHOOK_PORT_ENV: &str = "REMIND_ME_WEBHOOK_PORT";

pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8769;

/// Largest request body accepted, before anything is buffered.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Largest request line plus header block accepted.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// The only path that does anything.
pub const INGEST_PATH: &str = "/ingest";

/// Recent errors kept for the status report.
const ERROR_HISTORY: usize = 10;

/// How long the accept loop sleeps between polls when idle. Sets the worst
/// case for how long [`WebhookServer::stop`] takes to return.
const ACCEPT_POLL: Duration = Duration::from_millis(25);

/// Per-read and per-write deadline on a client connection.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Where to listen and what token to require.
///
/// Constructing one is the only way to enable the endpoint, and it cannot be
/// constructed without a non-empty secret.
#[derive(Clone)]
pub struct WebhookConfig {
    pub bind: String,
    pub port: u16,
    secret: String,
}

// Hand-written so the secret cannot reach a log line through a derived
// `Debug` on this or on anything holding it.
impl std::fmt::Debug for WebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl WebhookConfig {
    /// Read the configuration, or `None` when no secret is set.
    ///
    /// `None` is the disabled state, not an error: not configuring a push
    /// endpoint is the ordinary case.
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var(WEBHOOK_SECRET_ENV).ok()?;
        let bind = std::env::var(WEBHOOK_BIND_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BIND.to_string());
        let port = std::env::var(WEBHOOK_PORT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        Self::new(bind, port, secret)
    }

    /// Build a configuration directly.
    ///
    /// `None` when `secret` is empty — an unauthenticated endpoint is not a
    /// configuration this type can express.
    pub fn new(bind: impl Into<String>, port: u16, secret: impl Into<String>) -> Option<Self> {
        let secret = secret.into();
        if secret.is_empty() {
            return None;
        }
        Some(Self {
            bind: bind.into(),
            port,
            secret,
        })
    }

    /// The `Authorization` header value a request has to carry.
    fn expected_authorization(&self) -> String {
        format!("Bearer {}", self.secret)
    }
}

/// Compare two byte strings in time independent of where they first differ.
///
/// The whole point is the absence of an early return. A `==` on the token
/// would leak, through response latency, how many leading bytes a guess got
/// right — which turns recovering the secret from an exhaustive search into a
/// linear one. Lengths are compared into the same accumulator rather than
/// short-circuiting; the length itself is not secret, and treating it as such
/// would mean padding to a fixed size for no gain.
///
/// `black_box` keeps the optimiser from reintroducing an early exit it could
/// prove equivalent for a pure boolean result.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= (x ^ y) as usize;
    }
    std::hint::black_box(diff) == 0
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Counters {
    ingested: usize,
    skipped: usize,
    errored: usize,
    errors: VecDeque<String>,
}

/// Request tallies, shared between the serving thread and the status reader.
#[derive(Debug, Default)]
pub struct WebhookCounters(Mutex<Counters>);

impl WebhookCounters {
    fn record_ingested(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).ingested += 1;
    }

    fn record_skipped(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).skipped += 1;
    }

    fn record_error(&self, reason: &str) {
        let mut counters = self.0.lock().unwrap_or_else(|e| e.into_inner());
        counters.errored += 1;
        if counters.errors.len() == ERROR_HISTORY {
            counters.errors.pop_front();
        }
        counters.errors.push_back(reason.to_string());
    }

    /// `(ingested, skipped, errored, recent errors)`.
    pub fn snapshot(&self) -> (usize, usize, usize, Vec<String>) {
        let counters = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (
            counters.ingested,
            counters.skipped,
            counters.errored,
            counters.errors.iter().cloned().collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// What the `Content-Length` header said.
///
/// "Absent" and "present but unparseable" are different answers and get
/// different responses, so they are not collapsed into an `Option`.
#[derive(Debug, PartialEq, Eq)]
enum ContentLength {
    Absent,
    Invalid,
    Value(usize),
}

/// The request line and the headers this endpoint cares about.
#[derive(Debug)]
struct Head {
    method: String,
    path: String,
    authorization: String,
    content_length: ContentLength,
}

enum HeadOutcome {
    Complete(Head, Vec<u8>),
    /// The header block ran past [`MAX_HEAD_BYTES`].
    TooLarge,
    /// The client hung up, or sent something that is not an HTTP request line.
    Unusable,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read until the end of the header block, returning it plus whatever body
/// bytes arrived in the same read.
///
/// The cap is checked against what has been buffered, not against a declared
/// length, so a client that never sends the terminator cannot grow this
/// without bound.
fn read_head<R: Read>(stream: &mut R) -> io::Result<HeadOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    let split = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(HeadOutcome::TooLarge);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(HeadOutcome::Unusable);
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let body_prefix = buf.split_off(split + 4);
    buf.truncate(split);

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");

    let Some(request_line) = lines.next() else {
        return Ok(HeadOutcome::Unusable);
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(HeadOutcome::Unusable);
    };
    if method.is_empty() || target.is_empty() {
        return Ok(HeadOutcome::Unusable);
    }

    let mut authorization = String::new();
    let mut content_length = ContentLength::Absent;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.to_string(),
            "content-length" => {
                content_length = match value.parse::<usize>() {
                    Ok(n) => ContentLength::Value(n),
                    Err(_) => ContentLength::Invalid,
                }
            }
            _ => {}
        }
    }

    // A query string is not part of the routing decision, and leaving it on
    // would make `/ingest?x=1` a 404.
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or(target)
        .to_string();

    Ok(HeadOutcome::Complete(
        Head {
            method: method.to_string(),
            path,
            authorization,
            content_length,
        },
        body_prefix,
    ))
}

/// Read and discard a pending body before an early rejection.
///
/// Closing a connection with unread bytes still in the receive buffer makes
/// the peer see a connection reset instead of the response just written, so a
/// rejected sender would get "connection reset" rather than "401". Bounded by
/// [`MAX_BODY_BYTES`] no matter what the client declared — this runs *before*
/// authentication, so the declared length is entirely untrusted.
fn drain_body<R: Read>(stream: &mut R, declared: &ContentLength, already_read: usize) {
    let ContentLength::Value(length) = declared else {
        return;
    };
    let mut remaining = (*length).min(MAX_BODY_BYTES).saturating_sub(already_read);
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => return,
            Ok(read) => remaining -= read,
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

fn write_response<W: Write>(stream: &mut W, status: u16, body: &Value) -> io::Result<()> {
    write_response_with(stream, status, body, "")
}

/// As [`write_response`], with `extra` inserted verbatim into the header block
/// — used only for `Retry-After` on a 429, which is defined in whole seconds
/// and is what tells a rejected client when to come back.
fn write_response_with<W: Write>(
    stream: &mut W,
    status: u16,
    body: &Value,
    extra: &str,
) -> io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         {extra}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        status,
        reason_phrase(status),
        payload.len(),
        extra = extra,
    )?;
    stream.write_all(&payload)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Payload validation
// ---------------------------------------------------------------------------

/// A validated `/ingest` body.
#[derive(Debug)]
pub struct IngestRequest {
    pub filename: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub extract_mode: String,
    pub max_length: usize,
    pub kind: ImportKind,
}

/// Check a decoded body, returning the request or the reason it was refused.
///
/// Deliberately strict about shapes rather than lenient: a push that names a
/// field this does not understand is far more likely to be a sender bug than
/// an intentional extension, and failing at the boundary with a specific
/// message beats failing four layers into the import pipeline with a generic
/// one.
pub fn validate_payload(payload: &Value) -> std::result::Result<IngestRequest, String> {
    let Some(object) = payload.as_object() else {
        return Err("invalid ingest payload".to_string());
    };

    let filename = match object.get("filename").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => return Err("missing or invalid 'filename'".to_string()),
    };

    let content = match object.get("content").and_then(Value::as_str) {
        Some(content) => content.to_string(),
        None => {
            return Err("missing or invalid 'content' (must be a UTF-8 text string)".to_string())
        }
    };

    let kind = match object.get("kind") {
        None | Some(Value::Null) => ImportKind::Auto,
        Some(Value::String(k)) => match k.as_str() {
            "auto" => ImportKind::Auto,
            "chat" => ImportKind::Chat,
            "document" => ImportKind::Document,
            other => {
                return Err(format!(
                    "invalid kind: {:?} (use 'auto', 'chat', or 'document')",
                    other
                ))
            }
        },
        Some(other) => {
            return Err(format!(
                "invalid kind: {} (use 'auto', 'chat', or 'document')",
                other
            ))
        }
    };

    let category = match object.get("category") {
        None | Some(Value::Null) => crate::importer::CHAT_SOURCE.to_string(),
        Some(Value::String(c)) => c.clone(),
        Some(_) => return Err("'category' must be a string".to_string()),
    };

    let tags = match object.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(tag) => tags.push(tag.to_string()),
                    None => return Err("'tags' must be a list of strings".to_string()),
                }
            }
            tags
        }
        Some(_) => return Err("'tags' must be a list of strings".to_string()),
    };

    let extract_mode = match object.get("extract_mode") {
        None | Some(Value::Null) => "assistant_messages".to_string(),
        Some(Value::String(mode)) => mode.clone(),
        Some(_) => return Err("'extract_mode' must be a string".to_string()),
    };

    let bounds = format!(
        "'max_length' must be an integer between {} and {}",
        crate::models::IMPORT_MAX_LENGTH_MIN,
        crate::models::IMPORT_MAX_LENGTH_MAX
    );
    let max_length = match object.get("max_length") {
        None | Some(Value::Null) => 10_000,
        // `as_u64` rejects a float or a bool, which is the point: `true` and
        // `1000.5` are both wrong in ways that would otherwise coerce.
        Some(value) => match value.as_u64().map(|n| n as usize) {
            Some(n)
                if (crate::models::IMPORT_MAX_LENGTH_MIN
                    ..=crate::models::IMPORT_MAX_LENGTH_MAX)
                    .contains(&n) =>
            {
                n
            }
            _ => return Err(bounds),
        },
    };

    Ok(IngestRequest {
        filename,
        content,
        category,
        tags,
        extract_mode,
        max_length,
        kind,
    })
}

/// Marker written into every chunk's metadata by a pushed import.
///
/// **Not** the `source` column. `source` stays `chat_import`/`document_import`
/// exactly as a file import sets it, because that column feeds dedup, the
/// `normalize_batch` selection, and the vitality source prior — and a database
/// is meant to be readable by `remind_me`, which stores the same content under
/// those values whether it arrived by file or by push. Diverging there would
/// make identical content score differently depending on which implementation
/// happened to receive it. This records the arrival channel where recording it
/// costs nothing.
pub const INGEST_MARKER: &str = "webhook";

/// Stamp `metadata.ingest` on every chunk of an import.
fn mark_ingest_channel(conn: &Connection, import_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE memories SET metadata = json_set(metadata, '$.ingest', ?) WHERE doc_id = ?",
        rusqlite::params![INGEST_MARKER, import_id],
    )
}

/// Import a validated push, and record the outcome in the counters.
///
/// Returns the HTTP status and the body to send. `422` for a refused import
/// rather than `400`: the request was well-formed, it was the content that
/// could not be used.
fn ingest(conn: &Connection, request: &IngestRequest, counters: &WebhookCounters) -> (u16, Value) {
    let mut span = crate::telemetry::maybe_span("webhook.ingest");
    let outcome = import_bytes(
        conn,
        request.content.as_bytes(),
        &request.filename,
        &request.category,
        &request.tags,
        &request.extract_mode,
        request.max_length,
        request.kind,
    );

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            span.mark_error();
            let reason = e.to_string();
            counters.record_error(&reason);
            return (
                422,
                json!({ "status": "error", "reason": reason, "file": request.filename }),
            );
        }
    };

    match &outcome {
        ImportOutcome::Imported { import_id, .. } => {
            if let Err(e) = mark_ingest_channel(conn, import_id) {
                // The memories are already stored and searchable; only the
                // arrival marker is missing. Recording that as a failed import
                // would be worse than noting it.
                counters.record_error(&format!("ingest marker not written: {}", e));
            }
            counters.record_ingested();
            (
                200,
                serde_json::to_value(&outcome).unwrap_or_else(|_| json!({})),
            )
        }
        ImportOutcome::Skipped { .. } => {
            counters.record_skipped();
            (
                200,
                serde_json::to_value(&outcome).unwrap_or_else(|_| json!({})),
            )
        }
        ImportOutcome::Failed { reason, .. } => {
            counters.record_error(reason);
            (
                422,
                serde_json::to_value(&outcome).unwrap_or_else(|_| json!({})),
            )
        }
    }
}

/// Handle one request on an already-accepted connection.
///
/// Generic over the stream so the protocol can be exercised without a socket.
///
/// The order of the checks is the security-relevant part: **authentication
/// comes before routing**, so an unauthenticated caller cannot learn which
/// paths exist by comparing a 404 against a 405. Everything an unauthenticated
/// request can distinguish is a 401.
pub fn serve_once<S: Read + Write>(
    stream: &mut S,
    config: &WebhookConfig,
    conn: &Connection,
    counters: &WebhookCounters,
) -> io::Result<()> {
    serve_once_from(stream, config, conn, counters, "")
}

/// [`serve_once`] told who is calling, so the rate limiter can bucket by
/// address. An empty `peer_addr` shares one `ip:unknown` bucket rather than
/// bypassing the limit — an unidentifiable caller is exactly the one not to
/// exempt.
pub fn serve_once_from<S: Read + Write>(
    stream: &mut S,
    config: &WebhookConfig,
    conn: &Connection,
    counters: &WebhookCounters,
    peer_addr: &str,
) -> io::Result<()> {
    let (head, body_prefix) = match read_head(stream)? {
        HeadOutcome::Complete(head, prefix) => (head, prefix),
        HeadOutcome::TooLarge => {
            return write_response(
                stream,
                431,
                &json!({ "error": "request headers too large" }),
            )
        }
        // Nothing to reply to.
        HeadOutcome::Unusable => return Ok(()),
    };

    // Checked *before* authentication, deliberately. This endpoint is
    // reachable from the internet when tunnelled, and a limiter that only
    // engages after a valid credential would leave an unauthenticated flood
    // entirely unbounded — which is the flood that matters.
    let bucket = crate::rate_limit::resolve_key(
        head.authorization
            .strip_prefix("Bearer ")
            .unwrap_or(&head.authorization),
        peer_addr,
        Some(config.secret.as_str()),
    );
    if let Some(verdict) = crate::rate_limit::check(&bucket) {
        if !verdict.allowed {
            drain_body(stream, &head.content_length, body_prefix.len());
            let retry = crate::rate_limit::retry_after_seconds(verdict.retry_after);
            return write_response_with(
                stream,
                429,
                &json!({ "error": "rate limit exceeded" }),
                &format!("Retry-After: {}\r\n", retry),
            );
        }
    }

    if !constant_time_eq(
        head.authorization.as_bytes(),
        config.expected_authorization().as_bytes(),
    ) {
        drain_body(stream, &head.content_length, body_prefix.len());
        return write_response(stream, 401, &json!({ "error": "unauthorized" }));
    }

    if head.method != "POST" {
        drain_body(stream, &head.content_length, body_prefix.len());
        let status = if head.method == "GET" { 404 } else { 405 };
        return write_response(stream, status, &json!({ "error": "not found" }));
    }

    if head.path != INGEST_PATH {
        drain_body(stream, &head.content_length, body_prefix.len());
        return write_response(stream, 404, &json!({ "error": "not found" }));
    }

    let length = match head.content_length {
        ContentLength::Invalid => {
            return write_response(stream, 400, &json!({ "error": "invalid content-length" }))
        }
        ContentLength::Absent => {
            return write_response(stream, 400, &json!({ "error": "missing request body" }))
        }
        ContentLength::Value(0) => {
            return write_response(stream, 400, &json!({ "error": "missing request body" }))
        }
        ContentLength::Value(n) if n > MAX_BODY_BYTES => {
            drain_body(stream, &head.content_length, body_prefix.len());
            return write_response(stream, 413, &json!({ "error": "request body too large" }));
        }
        ContentLength::Value(n) => n,
    };

    // Capacity comes from the cap-checked length, so a lying `Content-Length`
    // reserves at most `MAX_BODY_BYTES` and a short body just reads short.
    let mut body = body_prefix;
    body.truncate(length);
    body.reserve(length.saturating_sub(body.len()));
    let mut chunk = [0u8; 8192];
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want])? {
            0 => break,
            read => body.extend_from_slice(&chunk[..read]),
        }
    }
    if body.len() < length {
        return write_response(stream, 400, &json!({ "error": "truncated request body" }));
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return write_response(stream, 400, &json!({ "error": "malformed JSON" })),
    };

    let request = match validate_payload(&payload) {
        Ok(request) => request,
        Err(reason) => return write_response(stream, 400, &json!({ "error": reason })),
    };

    let (status, response) = ingest(conn, &request, counters);
    write_response(stream, status, &response)
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// A listening endpoint and the thread serving it.
pub struct WebhookServer {
    bind: String,
    /// The port actually bound, which is not the configured one when the
    /// configuration asked for port 0.
    port: u16,
    counters: Arc<WebhookCounters>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl WebhookServer {
    /// Bind and start serving.
    ///
    /// Fails if the address is already in use — which usually means another
    /// instance is already serving, and is reported rather than retried.
    fn start(config: WebhookConfig, db: Arc<Database>) -> io::Result<Self> {
        let listener = TcpListener::bind((config.bind.as_str(), config.port))?;
        // The accept loop polls so it can observe the shutdown flag; a
        // blocking accept would sit there until a connection arrived.
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let bind = config.bind.clone();

        let counters = Arc::new(WebhookCounters::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_counters = Arc::clone(&counters);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::Builder::new()
            .name("webhook-server".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                            {
                                let conn = db.conn();
                                // A protocol-level I/O error is the client's
                                // connection dying, not this server's problem;
                                // the loop takes the next one.
                                let peer = stream
                                    .peer_addr()
                                    .map(|a| a.ip().to_string())
                                    .unwrap_or_default();
                                let _ = serve_once_from(
                                    &mut stream,
                                    &config,
                                    &conn,
                                    &thread_counters,
                                    &peer,
                                );
                            }
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        // The listener itself is broken; polling it forever
                        // would spin.
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            bind,
            port,
            counters,
            shutdown,
            handle: Option::Some(handle),
        })
    }

    pub fn bind(&self) -> &str {
        &self.bind
    }

    /// The port actually bound.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop serving and wait for the thread to finish the request in flight.
    ///
    /// Idempotent. Returns only once the serving thread has exited, which is
    /// what makes it safe to close database connections afterwards.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Whether the serving thread is still up.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for WebhookServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The endpoint in whichever state it ended up in.
///
/// # Why this is an enum and not an `Option`
///
/// "There is no webhook because nobody configured one" and "there was supposed
/// to be a webhook and the port was taken" are different facts, and an
/// operator wondering why a push is failing needs to tell them apart. Folded
/// into an `Option`, both read as absent.
///
/// # Shutdown ordering (SE-07)
///
/// The serving thread holds an `Arc<Database>` and writes through it, so it
/// must stop before those connections close. Rather than leave that to a
/// remembered call order, [`WebhookServer`] stops the thread in `Drop`, and a
/// holder that also owns the [`Database`] declares this field *before* it:
/// Rust drops struct fields in declaration order, so the join happens first by
/// construction. [`Webhook::stop`] is still available for a caller that wants
/// to shut the endpoint down explicitly while keeping the database open.
pub enum Webhook {
    /// No secret configured. Not an error.
    Disabled,
    Running(WebhookServer),
    /// Configured, but the listener could not be bound.
    Failed {
        bind: String,
        port: u16,
        error: String,
    },
}

impl Webhook {
    /// Start from the environment. [`Webhook::Disabled`] when no secret is set.
    pub fn from_env(db: Arc<Database>) -> Self {
        match WebhookConfig::from_env() {
            Some(config) => Self::start(config, db),
            None => Self::Disabled,
        }
    }

    /// Start against an explicit configuration.
    pub fn start(config: WebhookConfig, db: Arc<Database>) -> Self {
        let (bind, port) = (config.bind.clone(), config.port);
        match WebhookServer::start(config, db) {
            Ok(server) => Self::Running(server),
            Err(e) => Self::Failed {
                bind,
                port,
                error: e.to_string(),
            },
        }
    }

    /// Stop the endpoint, if one is running. Idempotent.
    pub fn stop(&mut self) {
        if let Self::Running(server) = self {
            server.stop();
        }
    }

    pub fn status(&self) -> WebhookStatus {
        match self {
            Self::Disabled => WebhookStatus {
                enabled: false,
                running: false,
                bind: None,
                port: None,
                requests_ingested: 0,
                requests_skipped: 0,
                requests_errored: 0,
                recent_errors: Vec::new(),
                start_error: None,
                hint: Some(format!(
                    "set {} to enable push ingestion; it stays off without one because \
                     an unauthenticated endpoint that writes into memory is worse than none",
                    WEBHOOK_SECRET_ENV
                )),
            },
            Self::Failed { bind, port, error } => WebhookStatus {
                enabled: true,
                running: false,
                bind: Some(bind.clone()),
                port: Some(*port),
                requests_ingested: 0,
                requests_skipped: 0,
                requests_errored: 0,
                recent_errors: Vec::new(),
                start_error: Some(error.clone()),
                hint: Some(format!(
                    "configured but not listening; {}:{} could not be bound, which usually \
                     means another instance already has it",
                    bind, port
                )),
            },
            Self::Running(server) => {
                let (ingested, skipped, errored, recent_errors) = server.counters.snapshot();
                WebhookStatus {
                    enabled: true,
                    running: server.is_running(),
                    bind: Some(server.bind.clone()),
                    port: Some(server.port),
                    requests_ingested: ingested,
                    requests_skipped: skipped,
                    requests_errored: errored,
                    recent_errors,
                    start_error: None,
                    hint: None,
                }
            }
        }
    }
}

/// What `remind_me_webhook_status` reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookStatus {
    /// A secret is configured, so the endpoint is meant to exist.
    pub enabled: bool,
    /// It is actually listening.
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub requests_ingested: usize,
    pub requests_skipped: usize,
    pub requests_errored: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_errors: Vec<String>,
    /// Why binding failed. Present only in the `enabled && !running` case,
    /// which is what separates it from "nobody configured one".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}
