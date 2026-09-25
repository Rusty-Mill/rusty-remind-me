//! Coverage for the saved-search routes: `GET`/`POST /api/saved-searches`,
//! `GET /api/saved-searches/{name}/run`, `DELETE /api/saved-searches/{name}`.

mod common;
use common::{authed_get, authed_json, authed_server, call, get, server, unauthed_json, KEY};
use remind_me_api::ApiServer;
use serde_json::json;

fn add(server: &ApiServer, content: &str, tags: &[&str]) -> String {
    let response = authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content, "tags": tags }).to_string(),
    );
    response.json()["id"].as_str().unwrap().to_string()
}

fn save(server: &ApiServer, body: serde_json::Value) -> common::Response {
    authed_json(server, "POST", "/api/saved-searches", &body.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/saved-searches
// ---------------------------------------------------------------------------

#[test]
fn a_search_is_saved_with_its_filters() {
    let (server, root) = authed_server("saved-create");
    let response = save(
        &server,
        json!({
            "name": "open questions",
            "query": "unresolved",
            "category": "project",
            "tags": ["infra"],
            "watch": true,
        }),
    );
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["name"], "open questions");
    assert_eq!(body["query"], "unresolved");
    assert_eq!(body["filters"]["category"], "project");
    assert_eq!(body["filters"]["tags"][0], "infra");
    assert_eq!(body["watch"], true);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn re_saving_a_name_updates_it_in_place() {
    // Core defines a repeated name as an update, not a duplicate — so this
    // route must not end up with two rows, and must not answer 201 as though
    // it had created one.
    let (server, root) = authed_server("saved-update");
    save(&server, json!({ "name": "q", "query": "first" }));
    let second = save(&server, json!({ "name": "q", "query": "second" }));
    assert_eq!(second.status, 200);
    assert_eq!(second.json()["query"], "second");

    let listed = authed_get(&server, "/api/saved-searches");
    assert_eq!(listed.json()["count"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_missing_name_or_query_is_400() {
    let (server, root) = authed_server("saved-invalid");
    assert_eq!(save(&server, json!({ "query": "x" })).status, 400);
    assert_eq!(save(&server, json!({ "name": "x" })).status, 400);
    assert_eq!(
        save(&server, json!({ "name": " ", "query": "x" })).status,
        400
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn saving_is_a_write_and_is_refused_unauthenticated() {
    let (server, root) = server("saved-unauthed");
    let response = unauthed_json(
        &server,
        "POST",
        "/api/saved-searches",
        &json!({ "name": "q", "query": "x" }).to_string(),
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/saved-searches
// ---------------------------------------------------------------------------

#[test]
fn an_empty_store_lists_no_saved_searches() {
    let (server, root) = authed_server("saved-list-empty");
    let response = authed_get(&server, "/api/saved-searches");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn saved_searches_are_listed_alphabetically() {
    let (server, root) = authed_server("saved-list-order");
    save(&server, json!({ "name": "zebra", "query": "z" }));
    save(&server, json!({ "name": "alpha", "query": "a" }));

    let body = authed_get(&server, "/api/saved-searches").json();
    assert_eq!(body["count"], 2);
    assert_eq!(body["saved_searches"][0]["name"], "alpha");
    assert_eq!(body["saved_searches"][1]["name"], "zebra");
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/saved-searches/{name}/run
// ---------------------------------------------------------------------------

#[test]
fn running_a_saved_search_returns_its_current_matches() {
    let (server, root) = authed_server("saved-run");
    add(&server, "the boiler needs a service", &["house"]);
    add(&server, "nothing to do with heating", &["work"]);
    save(&server, json!({ "name": "boiler", "query": "boiler" }));

    let response = authed_get(&server, "/api/saved-searches/boiler/run");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["name"], "boiler");
    assert_eq!(body["query"], "boiler");
    assert_eq!(body["count"], 1);
    assert!(
        body["results"][0]["memory"]["content"]
            .as_str()
            .unwrap()
            .contains("boiler"),
        "the match is the boiler memory, got {:?}",
        response.body
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_name_with_a_space_is_percent_decoded_before_lookup() {
    let (server, root) = authed_server("saved-run-encoded");
    add(&server, "the boiler needs a service", &[]);
    save(
        &server,
        json!({ "name": "open questions", "query": "boiler" }),
    );

    let response = authed_get(&server, "/api/saved-searches/open%20questions/run");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["name"], "open questions");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn running_an_unknown_saved_search_is_404() {
    let (server, root) = authed_server("saved-run-unknown");
    let response = authed_get(&server, "/api/saved-searches/nowhere/run");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn running_is_a_read_and_stays_open_unauthenticated() {
    // Running a stored query is the same read `/api/memories/search` already
    // serves openly; gating one but not the other would be a distinction
    // without a difference.
    let (server, root) = server("saved-run-open");
    let response = get(&server, "/api/saved-searches/whatever/run");
    assert_eq!(response.status, 404, "404 for absence, not 401 for auth");
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// DELETE /api/saved-searches/{name}
// ---------------------------------------------------------------------------

#[test]
fn a_saved_search_is_deleted_by_name() {
    let (server, root) = authed_server("saved-delete");
    save(&server, json!({ "name": "q", "query": "x" }));

    let response = call(
        &server,
        "DELETE",
        "/api/saved-searches/q",
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        "",
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["deleted"], "q");
    assert_eq!(
        authed_get(&server, "/api/saved-searches").json()["count"],
        0
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_an_unknown_saved_search_is_404() {
    let (server, root) = authed_server("saved-delete-unknown");
    let response = call(
        &server,
        "DELETE",
        "/api/saved-searches/nowhere",
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        "",
    );
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_is_a_write_and_is_refused_unauthenticated() {
    let (server, root) = server("saved-delete-unauthed");
    let response = call(
        &server,
        "DELETE",
        "/api/saved-searches/q",
        None,
        Some("application/json"),
        "",
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}
