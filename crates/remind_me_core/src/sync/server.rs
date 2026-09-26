//! This node's own peer server: accepts a `push` from another node and
//! serves a `pull` request for one — the same protocol this node's own
//! sync worker uses against a configured hub, since (per the reference)
//! hub and peer are the same protocol against different endpoints. There is
//! deliberately no separate "hub mode": any node with a secret configured
//! can serve either role.
//!
//! Mirrors `webhook.rs`'s hand-rolled `TcpListener` server closely (bearer
//! auth, capped header/body reads, one connection at a time), duplicated
//! rather than shared: each hand-rolled HTTP surface in this crate is
//! self-contained, and this one additionally needs query-string parsing
//! `webhook.rs`'s single fixed-path endpoint never had to do.

use super::graph::apply_incoming_record;
use crate::db::stats::StoreStats;
use crate::db::sync_feed::SyncFeed;
use crate::Database;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub const HEALTH_PATH: &str = "/health";
pub const COUNT_PATH: &str = "/count";
pub const PUSH_PATH: &str = "/sync/push";
pub const PULL_PATH: &str = "/sync/pull";
pub const PULL_ENTITIES_PATH: &str = "/sync/pull_entities";
pub const PULL_LINKS_PATH: &str = "/sync/pull_links";
pub const PULL_ENTITY_RELATIONS_PATH: &str = "/sync/pull_entity_relations";

pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Server-side cap on a pull page, regardless of what a caller asks for.
pub const MAX_PULL_LIMIT: usize = 500;
const ACCEPT_POLL: Duration = Duration::from_millis(25);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PeerServerConfig {
    pub bind: String,
    pub port: u16,
    secret: String,
    node_id: String,
}

impl std::fmt::Debug for PeerServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerServerConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("node_id", &self.node_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl PeerServerConfig {
    /// Build a configuration directly, bypassing the environment — what
    /// tests use to exercise [`serve_once`] over a fake stream without
    /// touching process-global env vars.
    pub fn new(
        bind: impl Into<String>,
        port: u16,
        secret: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            bind: bind.into(),
            port,
            secret: secret.into(),
            node_id: node_id.into(),
        }
    }

    /// `None` when no secret is configured -- a surgical check independent
    /// of the full `sync_enabled()` gate (which also requires a hub URL,
    /// irrelevant to whether *this node* should accept inbound requests).
    pub fn from_env() -> Option<Self> {
        let secret = Some(super::configured_sync_secret()).filter(|s| !s.is_empty())?;
        let bind = std::env::var(super::PEER_BIND_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| super::DEFAULT_PEER_BIND.to_string());
        let port = std::env::var(super::PEER_PORT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(super::DEFAULT_PEER_PORT);
        Some(Self {
            bind,
            port,
            secret,
            node_id: super::configured_node_id(),
        })
    }

    fn expected_authorization(&self) -> String {
        format!("Bearer {}", self.secret)
    }
}

// ---------------------------------------------------------------------------
// Request parsing (head + query string)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ContentLength {
    Absent,
    Invalid,
    Value(usize),
}

struct Head {
    method: String,
    path: String,
    query: String,
    authorization: String,
    content_length: ContentLength,
}

enum HeadOutcome {
    Complete(Head, Vec<u8>),
    TooLarge,
    Unusable,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

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

    let mut target_parts = target.splitn(2, '?');
    let path = target_parts.next().unwrap_or(target).to_string();
    let query = target_parts.next().unwrap_or("").to_string();

    Ok(HeadOutcome::Complete(
        Head {
            method: method.to_string(),
            path,
            query,
            authorization,
            content_length,
        },
        body_prefix,
    ))
}

/// Percent-decode a query string value. The inverse of `pull.rs`'s
/// `urlencode` -- tolerant of a raw (un-encoded) byte too, since this only
/// ever needs to decode what this crate's own client sent.
fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| urldecode(v))
    })
}

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
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

fn write_response<W: Write>(stream: &mut W, status: u16, body: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason_phrase(status),
        payload.len()
    )?;
    stream.write_all(&payload)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn handle_health(config: &PeerServerConfig) -> Value {
    json!({
        "status": "ok",
        "node_id": config.node_id,
        // Published here as well as on the dashboard's own `/health` (#107):
        // a reconcile reports which build each side is running, and this is
        // where the other side reads it from.
        "version": crate::updater::INSTALLED_VERSION,
        "time": chrono::Utc::now().to_rfc3339(),
    })
}

/// Record counts, for a client comparing its own totals against this peer's.
///
/// Field-for-field the shape the hub's `GET /count` returns, so one
/// client-side comparator serves both. A peer-shaped variant would mean a
/// second copy of the diff logic, which is how the two would eventually
/// disagree about what "drift" means.
///
/// **`approximate` is always false, and always present.** A peer has no
/// planner estimates to offer — the hub's `?approx=1` is a Postgres
/// `reltuples` read with no SQLite equivalent — but a caller should not have
/// to know which kind of remote it is talking to in order to read the answer.
///
/// Counts deliberately do **not** filter `deleted_at`. Both ends of the
/// comparison have to count identically: the hub counts every row and reports
/// tombstones separately, so filtering here would make a healthy peer look
/// permanently behind by its own tombstone count.
fn handle_count(conn: &Connection, config: &PeerServerConfig) -> Result<Value, rusqlite::Error> {
    let feed = SyncFeed::new(conn);
    let (total, tombstones) = StoreStats::new(conn).memory_totals()?;

    Ok(json!({
        "role": "peer",
        "version": crate::updater::INSTALLED_VERSION,
        "node_id": config.node_id,
        "approximate": false,
        "memories": {
            "total": total,
            "live": total - tombstones,
            "tombstones": tombstones,
        },
        "entities": feed.entity_count()?,
        "memory_entities": feed.link_count()?,
        "entity_relations": feed.relation_count()?,
        "time": chrono::Utc::now().to_rfc3339(),
    }))
}

/// A push batch is naturally heterogeneous — every graph-table trigger
/// funnels into the same `sync_outbox`, so one page can carry `memory`,
/// `entity`, `entity_relation`, and `memory_entity` records together.
/// [`apply_incoming_record`] dispatches each on its own `record_type`.
fn handle_push(conn: &Connection, body: &[u8]) -> (u16, Value) {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({ "error": "malformed JSON" }));
    };
    let Some(records) = payload.get("records").and_then(Value::as_array) else {
        return (400, json!({ "error": "missing 'records' array" }));
    };

    let mut processed_ids = Vec::new();
    let mut failed = 0usize;
    for record_value in records {
        match apply_incoming_record(conn, record_value) {
            Ok(wire_id) => processed_ids.push(wire_id),
            Err(_) => failed += 1,
        }
    }

    (
        200,
        json!({ "accepted": processed_ids.len(), "processed_ids": processed_ids, "failed": failed }),
    )
}

fn handle_pull(conn: &Connection, query: &str) -> (u16, Value) {
    let since = query_param(query, "since")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    let since_id = query_param(query, "since_id").unwrap_or_default();
    let exclude_node = query_param(query, "exclude_node").filter(|s| !s.is_empty());
    let limit: usize = query_param(query, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_PULL_LIMIT)
        .clamp(1, MAX_PULL_LIMIT);

    let result =
        SyncFeed::new(conn).memories_after(&since, &since_id, exclude_node.as_deref(), limit);

    match result {
        Ok(records) => (200, json!({ "records": records, "count": records.len() })),
        Err(e) => (500, json!({ "error": e.to_string() })),
    }
}

/// `(since, since_id, limit)`, shared by all four pull endpoints.
fn cursor_params(query: &str) -> (String, String, usize) {
    let since = query_param(query, "since")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    let since_id = query_param(query, "since_id").unwrap_or_default();
    let limit: usize = query_param(query, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_PULL_LIMIT)
        .clamp(1, MAX_PULL_LIMIT);
    (since, since_id, limit)
}

/// `entities` pulls keyset-paged on `(updated_at, id)`, exactly like
/// `memories` — `exclude_node` is honored the same way.
fn handle_pull_entities(conn: &Connection, query: &str) -> (u16, Value) {
    let (since, since_id, limit) = cursor_params(query);
    let exclude_node = query_param(query, "exclude_node").filter(|s| !s.is_empty());

    let result =
        SyncFeed::new(conn).entities_after(&since, &since_id, exclude_node.as_deref(), limit);
    match result {
        Ok(records) => (200, json!({ "records": records, "count": records.len() })),
        Err(e) => (500, json!({ "error": e.to_string() })),
    }
}

/// `memory_entities` links have no `updated_at` (immutable) and no
/// single-column id, so this pages on `(created_at, memory_id||'|'||entity_id)`
/// — the same synthetic composite key the wire `id` field carries.
/// `exclude_node` is accepted (so an older client's query string doesn't 400)
/// but not applied — `memory_entities` has no `node_id` column at all,
/// matching the reference's own tolerated-but-unused parameter exactly.
fn handle_pull_links(conn: &Connection, query: &str) -> (u16, Value) {
    let (since, since_id, limit) = cursor_params(query);

    let result = SyncFeed::new(conn).links_after(&since, &since_id, limit);

    match result {
        Ok(records) => (200, json!({ "records": records, "count": records.len() })),
        Err(e) => (500, json!({ "error": e.to_string() })),
    }
}

/// `entity_relations` pulls keyset-paged on `(created_at, id)` — relations
/// already carry a real deterministic id, no synthetic key needed.
/// `exclude_node` is likewise accepted but unused, matching the reference.
fn handle_pull_entity_relations(conn: &Connection, query: &str) -> (u16, Value) {
    let (since, since_id, limit) = cursor_params(query);

    let result = SyncFeed::new(conn).relations_after(&since, &since_id, limit);

    match result {
        Ok(records) => (200, json!({ "records": records, "count": records.len() })),
        Err(e) => (500, json!({ "error": e.to_string() })),
    }
}

/// Handle one request on an already-accepted connection. Authentication
/// comes before routing, matching the webhook endpoint's own reasoning: an
/// unauthenticated caller must not be able to distinguish "wrong path" from
/// "wrong method" from "not authenticated" by comparing status codes.
pub fn serve_once<S: Read + Write>(
    stream: &mut S,
    config: &PeerServerConfig,
    conn: &Connection,
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
        HeadOutcome::Unusable => return Ok(()),
    };

    if !crate::webhook::constant_time_eq(
        head.authorization.as_bytes(),
        config.expected_authorization().as_bytes(),
    ) {
        drain_body(stream, &head.content_length, body_prefix.len());
        return write_response(stream, 401, &json!({ "error": "unauthorized" }));
    }

    match (head.method.as_str(), head.path.as_str()) {
        ("GET", HEALTH_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            write_response(stream, 200, &handle_health(config))
        }
        ("GET", COUNT_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            match handle_count(conn, config) {
                Ok(response) => write_response(stream, 200, &response),
                Err(e) => write_response(
                    stream,
                    500,
                    &json!({ "error": format!("count failed: {}", e) }),
                ),
            }
        }
        ("GET", PULL_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let (status, response) = handle_pull(conn, &head.query);
            write_response(stream, status, &response)
        }
        ("GET", PULL_ENTITIES_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let (status, response) = handle_pull_entities(conn, &head.query);
            write_response(stream, status, &response)
        }
        ("GET", PULL_LINKS_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let (status, response) = handle_pull_links(conn, &head.query);
            write_response(stream, status, &response)
        }
        ("GET", PULL_ENTITY_RELATIONS_PATH) => {
            drain_body(stream, &head.content_length, body_prefix.len());
            let (status, response) = handle_pull_entity_relations(conn, &head.query);
            write_response(stream, status, &response)
        }
        ("POST", PUSH_PATH) => {
            let length = match head.content_length {
                ContentLength::Invalid => {
                    return write_response(
                        stream,
                        400,
                        &json!({ "error": "invalid content-length" }),
                    )
                }
                ContentLength::Absent | ContentLength::Value(0) => {
                    return write_response(stream, 400, &json!({ "error": "missing request body" }))
                }
                ContentLength::Value(n) if n > MAX_BODY_BYTES => {
                    drain_body(stream, &head.content_length, body_prefix.len());
                    return write_response(
                        stream,
                        413,
                        &json!({ "error": "request body too large" }),
                    );
                }
                ContentLength::Value(n) => n,
            };
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
            let (status, response) = handle_push(conn, &body);
            write_response(stream, status, &response)
        }
        (method, path)
            if [
                HEALTH_PATH,
                COUNT_PATH,
                PULL_PATH,
                PUSH_PATH,
                PULL_ENTITIES_PATH,
                PULL_LINKS_PATH,
                PULL_ENTITY_RELATIONS_PATH,
            ]
            .contains(&path) =>
        {
            drain_body(stream, &head.content_length, body_prefix.len());
            let _ = method;
            write_response(stream, 405, &json!({ "error": "method not allowed" }))
        }
        _ => {
            drain_body(stream, &head.content_length, body_prefix.len());
            write_response(stream, 404, &json!({ "error": "not found" }))
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

pub struct PeerServer {
    bind: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PeerServer {
    fn start(config: PeerServerConfig, db: Arc<Database>) -> io::Result<Self> {
        let listener = TcpListener::bind((config.bind.as_str(), config.port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let bind = config.bind.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("sync-peer-server".to_string())
            .spawn(move || {
                // A connection of its own, not `db.conn()`'s shared `Mutex`:
                // `serve_once` can block for up to `IO_TIMEOUT` reading a
                // slow or hostile peer's request, and holding the
                // process-wide mutex for that span used to block every other
                // MCP tool call -- reads and writes alike -- until that peer
                // either finished or timed out. Mirrors `SyncWorker`'s own
                // fix for the identical shape of bug on the outbound side:
                // opened once and reused across accepted connections,
                // reopened on the next accept if the attempt itself failed.
                let mut conn = db.open_secondary().ok();
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                            if conn.is_none() {
                                conn = db.open_secondary().ok();
                            }
                            if let Some(c) = conn.as_ref() {
                                let _ = serve_once(&mut stream, &config, c);
                            }
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            bind,
            port,
            shutdown,
            handle: Some(handle),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn bind(&self) -> &str {
        &self.bind
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// This node's peer server in whichever state it ended up in — mirrors
/// `Webhook` exactly, including the same shutdown-ordering reasoning (a
/// holder that also owns the `Database` must declare this field before it).
pub enum SyncPeer {
    Disabled,
    Running(PeerServer),
    Failed {
        bind: String,
        port: u16,
        error: String,
    },
}

impl SyncPeer {
    pub fn from_env(db: Arc<Database>) -> Self {
        match PeerServerConfig::from_env() {
            Some(config) => {
                let (bind, port) = (config.bind.clone(), config.port);
                match PeerServer::start(config, db) {
                    Ok(server) => Self::Running(server),
                    Err(e) => Self::Failed {
                        bind,
                        port,
                        error: e.to_string(),
                    },
                }
            }
            None => Self::Disabled,
        }
    }

    pub fn stop(&mut self) {
        if let Self::Running(server) = self {
            server.stop();
        }
    }

    pub fn status(&self) -> PeerServerStatus {
        match self {
            Self::Disabled => PeerServerStatus {
                enabled: false,
                running: false,
                bind: None,
                port: None,
                start_error: None,
                hint: Some(format!(
                    "set {} (plus {} and {} to also push/pull as a client) to accept sync requests from other nodes",
                    super::SYNC_SECRET_ENV,
                    super::NODE_ID_ENV,
                    super::HUB_URL_ENV
                )),
            },
            Self::Failed { bind, port, error } => PeerServerStatus {
                enabled: true,
                running: false,
                bind: Some(bind.clone()),
                port: Some(*port),
                start_error: Some(error.clone()),
                hint: Some(format!("configured but not listening; {bind}:{port} could not be bound")),
            },
            Self::Running(server) => PeerServerStatus {
                enabled: true,
                running: server.is_running(),
                bind: Some(server.bind.clone()),
                port: Some(server.port),
                start_error: None,
                hint: None,
            },
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerServerStatus {
    pub enabled: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    // Shared with `sync::tests`, not a second module-local lock -- both
    // modules mutate `SYNC_SECRET_ENV` (a process-global), and two
    // independent locks over the same global would still race each other.
    // See that module's `ENV_LOCK` for the failure this fixes.
    use super::super::tests::ENV_LOCK;

    fn clear_env() {
        crate::test_env::remove_var(super::super::SYNC_SECRET_ENV);
        crate::test_env::remove_var(super::super::PEER_BIND_ENV);
        crate::test_env::remove_var(super::super::PEER_PORT_ENV);
    }

    /// A stray literal `${user_config.sync_secret}` string in the secret var
    /// -- what an MCP client's `env` config substitution would leave behind
    /// if it were ever attempted and failed to resolve -- must not read as
    /// "a secret is configured". Without routing through
    /// `configured_sync_secret`'s placeholder guard, this node would bind a
    /// peer server on `0.0.0.0` authenticated by a value baked into the
    /// plugin's own public source -- anyone who has read `.mcp.json` would
    /// already know the "secret". `from_env` must treat it as unconfigured
    /// instead.
    #[test]
    fn unresolved_user_config_placeholder_does_not_start_a_peer_server() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(super::super::SYNC_SECRET_ENV, "${user_config.sync_secret}");

        assert!(PeerServerConfig::from_env().is_none());

        clear_env();
    }

    #[test]
    fn a_real_secret_still_starts_a_peer_server() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(super::super::SYNC_SECRET_ENV, "s3cr3t");

        assert!(PeerServerConfig::from_env().is_some());

        clear_env();
    }
}
