//! Shared test harness: drives [`remind_me_api::ApiServer::serve_one`] over
//! an in-memory stream rather than a real socket, the same approach used for
//! the webhook endpoint's protocol tests — the whole point is to exercise
//! routing, auth and the content-type check without the flake a real
//! listener would add.
//!
//! Each `tests/*.rs` file compiles this module into its own separate test
//! binary (integration tests each get their own crate), so any one binary
//! only calls a subset of what is offered here — the rest is not dead code,
//! it is used by a sibling binary compiled from the same source.
//!
//! The `#![allow(dead_code)]` below is a deliberate module-level tradeoff,
//! not an oversight, and per-item allows were considered and rejected: with
//! thirteen test binaries in this crate drawing overlapping subsets of these
//! helpers, tagging each one with which binaries use it would drift out of
//! date every time a test file changed and become noise rather than
//! signal. Audited 2026-08-11 (issue #287) by grepping every helper name
//! across `crates/remind_me_api/tests/*.rs`: each item here is used by at
//! least one test binary today (e.g. `seeded_wiki_server` and `.header()`
//! only by `wiki_test.rs`; `raw_request` and `call_with_origin` only by
//! `dashboard_test.rs`; the rest by several). If a future audit finds a
//! helper unreferenced by every test binary, that is the real dead-code bug
//! this allow can hide -- remove the helper rather than assume it's fine.

#![allow(dead_code)]

use remind_me_api::ApiServer;
use remind_me_core::{wiki_fs::Wiki, Database};
use serde_json::Value;
use std::io::{Cursor, Read, Write};

pub const KEY: &str = "s3cret-api-key";

pub struct FakeStream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl FakeStream {
    pub fn new(request: impl Into<Vec<u8>>) -> Self {
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

/// A server over a fresh in-memory store, with a wiki rooted in its own
/// scratch directory — the default root is a real shared directory, and a
/// test using it would write into whatever wiki the machine's user has.
pub fn server(name: &str) -> (ApiServer, std::path::PathBuf) {
    seeded_server(name, |_conn| {})
}

/// Same as [`server`], but `seed` runs against the database before it is
/// handed to the server — for fixtures the HTTP surface itself has no route
/// to create (entities and relations have no HTTP write path, matching the
/// reference).
pub fn seeded_server(
    name: &str,
    seed: impl FnOnce(&rusqlite::Connection),
) -> (ApiServer, std::path::PathBuf) {
    seeded_wiki_server(name, |conn, _wiki| seed(conn))
}

/// Same as [`seeded_server`], but `seed` also gets the `Wiki` — for wiki-page
/// fixtures, which are files on disk plus an index row and so need both the
/// wiki root and the connection to create, unlike every other fixture in
/// this test suite.
pub fn seeded_wiki_server(
    name: &str,
    seed: impl FnOnce(&rusqlite::Connection, &Wiki),
) -> (ApiServer, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("rrm_api_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = Database::open_in_memory().unwrap();
    let wiki = Wiki::new(&root);
    seed(&db.conn(), &wiki);
    let server = ApiServer::with_wiki(db, wiki);
    (server, root)
}

/// Same as [`server`], but with `REMIND_ME_API_KEY` set to [`KEY`].
pub fn authed_server(name: &str) -> (ApiServer, std::path::PathBuf) {
    let (server, root) = server(name);
    (server.with_api_key(Some(KEY.to_string())), root)
}

pub fn raw_request(
    method: &str,
    path: &str,
    auth: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> String {
    raw_request_with_origin(method, path, None, auth, content_type, body)
}

/// As [`raw_request`], additionally carrying an `Origin` header when given
/// one — for CORS tests, which are the only ones that need a request to look
/// like it came from a browser tab rather than a script.
pub fn raw_request_with_origin(
    method: &str,
    path: &str,
    origin: Option<&str>,
    auth: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> String {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: localhost\r\n", method, path);
    if let Some(origin) = origin {
        head.push_str(&format!("Origin: {}\r\n", origin));
    }
    if let Some(auth) = auth {
        head.push_str(&format!("Authorization: {}\r\n", auth));
    }
    if let Some(content_type) = content_type {
        head.push_str(&format!("Content-Type: {}\r\n", content_type));
    }
    if !body.is_empty() || method != "GET" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    head.push_str(body);
    head
}

/// The full parsed response: status, every header (lowercased name), and the
/// raw body text.
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("response body was not JSON ({e}): {:?}", self.body))
    }

    /// A response header by name, case-insensitively — for CORS assertions,
    /// which care whether `Access-Control-Allow-Origin` is present at all.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

pub fn parse_response(raw: &[u8]) -> Response {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a response has a header block");
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers: std::collections::HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let content_type = headers.get("content-type").cloned().unwrap_or_default();
    Response {
        status,
        content_type,
        headers,
        body: body.to_string(),
    }
}

/// Run one request against `server`, returning the parsed response.
pub fn call(
    server: &ApiServer,
    method: &str,
    path: &str,
    auth: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> Response {
    call_full(server, method, path, None, auth, content_type, body)
}

/// As [`call`], additionally carrying an `Origin` header — CORS tests only.
pub fn call_with_origin(server: &ApiServer, method: &str, path: &str, origin: &str) -> Response {
    call_full(server, method, path, Some(origin), None, None, "")
}

/// The fully general form every other `call*` helper delegates to.
pub fn call_full(
    server: &ApiServer,
    method: &str,
    path: &str,
    origin: Option<&str>,
    auth: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> Response {
    let raw = raw_request_with_origin(method, path, origin, auth, content_type, body);
    let mut stream = FakeStream::new(raw.into_bytes());
    server.serve_one(&mut stream).expect("no I/O failure");
    parse_response(&stream.output)
}

/// A GET with no auth header — the common case for read-route tests against
/// an unauthenticated server.
pub fn get(server: &ApiServer, path: &str) -> Response {
    call(server, "GET", path, None, None, "")
}

/// A GET authenticated with [`KEY`] — for tests running against
/// [`authed_server`], where every route requires the token.
pub fn authed_get(server: &ApiServer, path: &str) -> Response {
    call(
        server,
        "GET",
        path,
        Some(&format!("Bearer {}", KEY)),
        None,
        "",
    )
}

/// A JSON-bodied mutating request, authenticated with [`KEY`].
pub fn authed_json(server: &ApiServer, method: &str, path: &str, body: &str) -> Response {
    call(
        server,
        method,
        path,
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        body,
    )
}

/// A JSON-bodied mutating request with no auth header, for testing the
/// unauthenticated-server posture.
pub fn unauthed_json(server: &ApiServer, method: &str, path: &str, body: &str) -> Response {
    call(server, method, path, None, Some("application/json"), body)
}
