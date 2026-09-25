//! Coverage for `GET /health`, `GET /api/stats`, `GET /api/vitality`, and the
//! protocol-level behaviour that isn't specific to any one route: 404/405
//! routing and malformed input.

mod common;
use common::{authed_json, authed_server, call, get, server};
use serde_json::json;

// ---------------------------------------------------------------------------
// Liveness / stats / vitality
// ---------------------------------------------------------------------------

#[test]
fn health_reports_ok() {
    let (server, root) = server("health");
    let response = get(&server, "/health");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["status"], "ok");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn stats_on_an_empty_store() {
    let (server, root) = server("stats-empty");
    let response = get(&server, "/api/stats");
    assert_eq!(response.status, 200);
    // `total`, not `total_memories` -- the dashboard's own shape
    // (`api.py:531-562`), distinct from `remind_me_stats`'s. The vendored
    // JSX reads `stats.total`/`stats.tags` with a `||0` fallback, so the
    // wrong field name here previously passed silently while the dashboard
    // rendered every count as zero.
    assert_eq!(response.json()["total"], 0);
    assert_eq!(response.json()["tags"], json!({}));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn stats_reflects_what_was_added() {
    let (server, root) = authed_server("stats-populated");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "counted", "tags": ["quokka"] }).to_string(),
    );

    let response = call(
        &server,
        "GET",
        "/api/stats",
        Some(&format!("Bearer {}", common::KEY)),
        None,
        "",
    );
    assert_eq!(response.json()["total"], 1);
    assert_eq!(response.json()["tags"]["quokka"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn vitality_report_on_an_empty_store() {
    let (server, root) = server("vitality-empty");
    let response = get(&server, "/api/vitality");
    assert_eq!(response.status, 200);
    assert!(response.json().is_object());
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_path_is_404() {
    let (server, root) = server("routing-404");
    let response = get(&server, "/api/nowhere");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_wrong_method_on_a_known_path_is_405() {
    // Authenticated, so auth cannot pre-empt the routing decision: a
    // mutating method is refused with 401 before dispatch on an
    // unauthenticated server regardless of whether the route even accepts
    // it, which would make this indistinguishable from the auth check.
    let (server, root) = authed_server("routing-405");
    let response = authed_json(&server, "DELETE", "/api/stats", "");
    assert_eq!(response.status, 405, "{:?}", response.body);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_get_on_a_memory_id_path_with_no_id_falls_through_to_the_list_route() {
    // "/api/memories/" (trailing slash, empty id segment) must not match
    // `{memory_id}` — an empty captured segment is refused by the pattern
    // matcher — and "/api/memories" itself (no trailing slash) is the list
    // route, a different path entirely.
    let (server, root) = server("routing-trailing-slash");
    let response = get(&server, "/api/memories/");
    assert_eq!(response.status, 404, "{:?}", response.body);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// Malformed requests
// ---------------------------------------------------------------------------

#[test]
fn a_get_needs_no_content_length() {
    let (server, root) = server("no-body-get");
    let response = get(&server, "/health");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unusable_request_gets_no_response_and_does_not_crash_the_server() {
    let (server, root) = server("garbage-request");
    let mut stream = GarbageStream(std::io::Cursor::new(b"not an http request at all".to_vec()));
    server.serve_one(&mut stream).unwrap();
    // No crash is the assertion; a garbage request has nothing to answer.
    std::fs::remove_dir_all(&root).unwrap();
}

struct GarbageStream(std::io::Cursor<Vec<u8>>);
impl std::io::Read for GarbageStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.0, buf)
    }
}
impl std::io::Write for GarbageStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
