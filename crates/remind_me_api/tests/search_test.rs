//! Coverage for `GET /api/memories/search`.

mod common;
use common::{authed_get, authed_json, authed_server};
use serde_json::json;

fn add_with(server: &remind_me_api::ApiServer, body: serde_json::Value) -> String {
    authed_json(server, "POST", "/api/memories", &body.to_string()).json()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_missing_query_is_a_400() {
    let (server, root) = authed_server("search-no-q");
    let response = authed_get(&server, "/api/memories/search");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_store_returns_no_hits() {
    let (server, root) = authed_server("search-empty");
    let response = authed_get(&server, "/api/memories/search?q=quokka");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["total"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_populated_store_ranks_matches() {
    // FTS5 does not stem: the query word must appear as written.
    let (server, root) = authed_server("search-populated");
    add_with(
        &server,
        json!({ "content": "quokka lives on Rottnest Island" }),
    );
    add_with(&server, json!({ "content": "unrelated content" }));

    let response = authed_get(&server, "/api/memories/search?q=quokka");
    let body = response.json();
    assert_eq!(body["total"], 1);
    assert_eq!(
        body["memories"][0]["content"],
        "quokka lives on Rottnest Island"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn category_and_tags_filter_results() {
    let (server, root) = authed_server("search-filters");
    add_with(
        &server,
        json!({ "content": "quokka one", "category": "wildlife" }),
    );
    add_with(
        &server,
        json!({ "content": "quokka two", "category": "general" }),
    );

    let response = authed_get(&server, "/api/memories/search?q=quokka&category=wildlife");
    assert_eq!(response.json()["total"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn search_pages_with_the_standard_envelope() {
    let (server, root) = authed_server("search-paging");
    for i in 0..3 {
        add_with(
            &server,
            json!({ "content": format!("quokka sighting {}", i) }),
        );
    }

    let response = authed_get(&server, "/api/memories/search?q=quokka&limit=2");
    let body = response.json();
    assert_eq!(body["count"], 2);
    assert_eq!(body["has_more"], true);
    std::fs::remove_dir_all(&root).unwrap();
}

// An `entity:` token that resolves to a real, linked entity is covered at
// the core layer (`search_page_test.rs`'s
// `an_entity_token_narrows_to_linked_or_spo_matching_memories`) — this HTTP
// surface has no route that creates an entity link (neither does the
// reference's), so there is no way to set that fixture up through HTTP
// alone. What the HTTP layer adds on top — extracting the token from `q` and
// wiring it through — is exercised here via the unknown-entity path, which
// only needs a request, no fixture.

#[test]
fn an_unknown_entity_is_an_empty_page_with_a_message_not_an_error() {
    let (server, root) = authed_server("search-unknown-entity");
    add_with(&server, json!({ "content": "quokka sighting" }));

    let response = authed_get(&server, "/api/memories/search?q=entity:Nowhere");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["total"], 0);
    assert!(body["message"].as_str().unwrap().contains("Nowhere"));
    std::fs::remove_dir_all(&root).unwrap();
}
