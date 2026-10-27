//! Coverage for `GET /api/brief`, the REST form of `remind_me_brief`.

mod common;
use common::{authed_get, authed_json, authed_server};
use serde_json::json;

#[test]
fn a_brief_scopes_by_tag_and_lists_sections() {
    let (server, root) = authed_server("brief-basic");
    let added = authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "quokka parser shipped", "tags": ["quokka"] }).to_string(),
    );
    let id = added.json()["id"].as_str().unwrap().to_string();
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "unrelated", "tags": ["wombat"] }).to_string(),
    );

    let response = authed_get(&server, "/api/brief?project=quokka&since=2d");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["sections"][2]["name"], "activity");
    assert_eq!(body["sections"][2]["items"][0]["id"], json!(id));
    assert_eq!(body["sections"][2]["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["project"]["matched_by"], json!(["tag"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn bad_input_is_a_400_and_an_unknown_project_a_404() {
    let (server, root) = authed_server("brief-errors");
    assert_eq!(authed_get(&server, "/api/brief").status, 400);
    assert_eq!(
        authed_get(&server, "/api/brief?project=x&since=soon").status,
        400
    );
    assert_eq!(
        authed_get(&server, "/api/brief?project=x&token_budget=5").status,
        400
    );
    let unknown = authed_get(&server, "/api/brief?project=nowhere");
    assert_eq!(unknown.status, 404);
    assert!(unknown.json()["suggestions"].is_array());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_response_body_stays_within_a_minimal_budget() {
    let (server, root) = authed_server("brief-budget");
    for i in 0..20 {
        authed_json(
            &server,
            "POST",
            "/api/memories",
            &json!({ "content": format!("quokka {i} {}", "word ".repeat(30)), "tags": ["quokka"] })
                .to_string(),
        );
    }
    let body = authed_get(&server, "/api/brief?project=quokka&token_budget=200").json();
    let tokens = remind_me_core::retrieval::estimated_content_tokens(&body.to_string());
    assert!(tokens <= 200, "{tokens} tokens");
    assert!(body["sections"][2]["omitted"].as_u64().unwrap() > 0);
    std::fs::remove_dir_all(&root).unwrap();
}
