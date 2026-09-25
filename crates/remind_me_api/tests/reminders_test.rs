//! Coverage for `GET /api/reminders` and `POST /api/reminders` — the HTTP
//! surface over `remind_me_set_reminder` / `remind_me_list_reminders`.
//!
//! Distinct from `reminders_ics_test.rs`, which covers the token-authenticated
//! calendar feed at `/api/reminders/{token}.ics`. Both read the same core
//! query; only these routes can write one.

mod common;
use common::{authed_get, authed_json, authed_server, server, unauthed_json};
use remind_me_api::ApiServer;
use serde_json::json;

/// Far enough out that this test suite does not acquire an expiry date. A
/// literal beats `Utc::now() + Duration` here: the assertion is about the
/// route's handling of a future timestamp, not about arithmetic.
const FUTURE: &str = "2999-01-01T09:00:00Z";

fn add(server: &ApiServer, content: &str) -> String {
    let response = authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content }).to_string(),
    );
    response.json()["id"].as_str().unwrap().to_string()
}

fn set_reminder(server: &ApiServer, body: serde_json::Value) -> common::Response {
    authed_json(server, "POST", "/api/reminders", &body.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/reminders
// ---------------------------------------------------------------------------

#[test]
fn a_reminder_is_set_on_a_live_memory() {
    let (server, root) = authed_server("reminder-set");
    let id = add(&server, "renew the certificate");

    let response = set_reminder(&server, json!({ "memory_id": id, "remind_at": FUTURE }));
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["outcome"], "set");
    assert_eq!(body["memory_id"], id);
    // Canonicalized to UTC by core, which is what was actually stored.
    assert!(
        body["remind_at"]
            .as_str()
            .unwrap()
            .starts_with("2999-01-01"),
        "the stored timestamp is echoed back, got {:?}",
        response.body
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn omitting_remind_at_clears_an_existing_reminder() {
    let (server, root) = authed_server("reminder-clear");
    let id = add(&server, "renew the certificate");
    set_reminder(&server, json!({ "memory_id": id, "remind_at": FUTURE }));

    let cleared = set_reminder(&server, json!({ "memory_id": id }));
    assert_eq!(cleared.status, 200);
    assert_eq!(cleared.json()["outcome"], "cleared");

    // And it really is gone from the window, not merely reported as cleared.
    let listed = authed_get(&server, "/api/reminders?when=all");
    assert_eq!(listed.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_remind_at_clears_rather_than_failing() {
    // What a cleared <input type="datetime-local"> sends. Core treats blank as
    // "clear", and this route must not turn that into a parse error.
    let (server, root) = authed_server("reminder-clear-blank");
    let id = add(&server, "renew the certificate");
    set_reminder(&server, json!({ "memory_id": id, "remind_at": FUTURE }));

    let cleared = set_reminder(&server, json!({ "memory_id": id, "remind_at": "" }));
    assert_eq!(cleared.status, 200);
    assert_eq!(cleared.json()["outcome"], "cleared");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_past_timestamp_is_refused_with_400() {
    let (server, root) = authed_server("reminder-past");
    let id = add(&server, "renew the certificate");

    let response = set_reminder(
        &server,
        json!({ "memory_id": id, "remind_at": "2000-01-01T00:00:00Z" }),
    );
    assert_eq!(response.status, 400);
    let body = response.json();
    assert_eq!(body["outcome"], "rejected");
    assert!(
        body["reason"].as_str().unwrap().contains("future"),
        "the refusal says why, got {:?}",
        response.body
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unparseable_timestamp_is_refused_with_400() {
    let (server, root) = authed_server("reminder-garbage");
    let id = add(&server, "renew the certificate");

    let response = set_reminder(
        &server,
        json!({ "memory_id": id, "remind_at": "next tuesday" }),
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.json()["outcome"], "rejected");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_memory_is_404() {
    let (server, root) = authed_server("reminder-unknown");
    let response = set_reminder(
        &server,
        json!({ "memory_id": "mem_nope", "remind_at": FUTURE }),
    );
    assert_eq!(response.status, 404);
    assert_eq!(response.json()["outcome"], "not_found");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_body_without_memory_id_is_400() {
    let (server, root) = authed_server("reminder-no-id");
    let response = set_reminder(&server, json!({ "remind_at": FUTURE }));
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("memory_id"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn setting_a_reminder_is_a_write_and_is_refused_unauthenticated() {
    let (server, root) = server("reminder-unauthed");
    let response = unauthed_json(
        &server,
        "POST",
        "/api/reminders",
        &json!({ "memory_id": "mem_x", "remind_at": FUTURE }).to_string(),
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/reminders
// ---------------------------------------------------------------------------

#[test]
fn the_default_window_is_upcoming() {
    let (server, root) = authed_server("reminder-list-default");
    let id = add(&server, "renew the certificate");
    set_reminder(&server, json!({ "memory_id": id, "remind_at": FUTURE }));

    let response = authed_get(&server, "/api/reminders");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["count"], 1);
    assert_eq!(body["memories"][0]["id"], id);
    assert!(body["memories"][0]["remind_at"].is_string());

    // The same memory is not overdue, so the other window is empty — which is
    // what makes this an assertion about windowing rather than about counting.
    let overdue = authed_get(&server, "/api/reminders?when=overdue");
    assert_eq!(overdue.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_memory_with_no_reminder_is_not_listed() {
    let (server, root) = authed_server("reminder-list-none");
    add(&server, "no reminder here");
    let response = authed_get(&server, "/api/reminders?when=all");
    assert_eq!(response.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_window_is_refused_rather_than_defaulted() {
    // A typo'd window that quietly answered "upcoming" would read as "no
    // overdue reminders", which is the wrong answer told convincingly.
    let (server, root) = authed_server("reminder-bad-window");
    let response = authed_get(&server, "/api/reminders?when=someday");
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("upcoming"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_non_numeric_limit_is_400() {
    let (server, root) = authed_server("reminder-bad-limit");
    let response = authed_get(&server, "/api/reminders?limit=lots");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn listing_reminders_is_open_on_an_unauthenticated_server() {
    let (server, root) = server("reminder-list-open");
    let response = common::get(&server, "/api/reminders");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
