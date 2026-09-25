//! Coverage for the auth posture described in `lib.rs`'s module docs.
//!
//! The posture is asymmetric by design: unauthenticated reads are this
//! crate's pre-existing behaviour, carried forward explicitly rather than
//! inherited silently; unauthenticated *writes* are refused, because adding
//! mutating routes is exactly the change that must not default open.

mod common;
use common::{authed_server, call, get, server, unauthed_json, KEY};

#[test]
fn health_is_always_public_with_no_key_configured() {
    let (server, root) = server("health-open");
    let response = get(&server, "/health");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["status"], "ok");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn health_is_always_public_even_when_a_key_is_configured() {
    let (server, root) = authed_server("health-authed");
    // No Authorization header at all.
    let response = get(&server, "/health");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_get_route_is_open_when_no_key_is_configured() {
    let (server, root) = server("get-open");
    let response = get(&server, "/api/stats");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_mutating_route_is_refused_with_no_key_configured() {
    let (server, root) = server("write-closed");
    let response = unauthed_json(&server, "POST", "/api/memories", r#"{"content":"x"}"#);
    assert_eq!(response.status, 401);
    assert!(
        response.json()["error"]
            .as_str()
            .unwrap()
            .contains("REMIND_ME_API_KEY"),
        "the refusal names the fix, got {:?}",
        response.body
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn every_mutating_method_is_refused_without_a_key() {
    let (server, root) = server("write-closed-methods");
    for (method, path) in [
        ("POST", "/api/memories"),
        ("PUT", "/api/memories/mem_x"),
        ("PATCH", "/api/memories/mem_x"),
        ("DELETE", "/api/memories/mem_x"),
        ("POST", "/api/import"),
        ("POST", "/api/memories/bulk/delete"),
    ] {
        let response = unauthed_json(&server, method, path, "{}");
        assert_eq!(
            response.status, 401,
            "{} {} should be refused",
            method, path
        );
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn once_a_key_is_configured_every_route_requires_it_including_reads() {
    let (server, root) = authed_server("full-lockdown");
    let response = get(&server, "/api/stats");
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_correct_bearer_token_is_accepted() {
    let (server, root) = authed_server("correct-token");
    let response = call(
        &server,
        "GET",
        "/api/stats",
        Some(&format!("Bearer {}", KEY)),
        None,
        "",
    );
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_wrong_bearer_token_is_refused() {
    let (server, root) = authed_server("wrong-token");
    let response = call(&server, "GET", "/api/stats", Some("Bearer nope"), None, "");
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_token_missing_the_bearer_prefix_is_refused() {
    let (server, root) = authed_server("missing-prefix");
    let response = call(&server, "GET", "/api/stats", Some(KEY), None, "");
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_mutating_route_still_needs_the_content_type_check_once_authenticated() {
    let (server, root) = authed_server("still-needs-ct");
    let response = call(
        &server,
        "POST",
        "/api/memories",
        Some(&format!("Bearer {}", KEY)),
        Some("text/plain"),
        r#"{"content":"x"}"#,
    );
    // Passing auth does not skip the CSRF-hardening content-type check.
    assert_eq!(response.status, 415);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn auth_is_checked_before_content_type_so_an_unauthenticated_write_cannot_map_the_gate() {
    let (server, root) = server("order-check");
    // No key configured (writes are refused outright) *and* a bad
    // content-type: if content-type ran first this would be 415, which
    // would let an unauthenticated caller learn something about the route
    // before being told no.
    let response = call(
        &server,
        "POST",
        "/api/memories",
        None,
        Some("text/plain"),
        "{}",
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_json_content_type_with_a_charset_suffix_is_accepted() {
    let (server, root) = authed_server("ct-charset");
    let response = call(
        &server,
        "POST",
        "/api/memories",
        Some(&format!("Bearer {}", KEY)),
        Some("application/json; charset=utf-8"),
        r#"{"content":"x"}"#,
    );
    assert_eq!(response.status, 201, "{:?}", response.body);
    std::fs::remove_dir_all(&root).unwrap();
}
