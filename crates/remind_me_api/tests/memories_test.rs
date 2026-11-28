//! Coverage for `GET/POST /api/memories`, `GET/PUT/PATCH/DELETE
//! /api/memories/{id}`.

mod common;
use common::{authed_get, authed_json, authed_server};
use remind_me_api::ApiServer;
use serde_json::json;

fn add(server: &ApiServer, content: &str) -> String {
    let response = authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content }).to_string(),
    );
    response.json()["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// GET /api/memories
// ---------------------------------------------------------------------------

#[test]
fn an_empty_store_lists_nothing() {
    let (server, root) = authed_server("list-empty");
    let response = authed_get(&server, "/api/memories");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["total"], 0);
    assert_eq!(body["count"], 0);
    assert!(body["memories"].as_array().unwrap().is_empty());
    assert_eq!(body["has_more"], false);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_populated_store_lists_newest_first() {
    let (server, root) = authed_server("list-populated");
    add(&server, "first");
    add(&server, "second");

    let response = authed_get(&server, "/api/memories");
    let body = response.json();
    assert_eq!(body["total"], 2);
    assert_eq!(body["memories"][0]["content"], "second");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn category_source_and_tags_filter_the_list() {
    let (server, root) = authed_server("list-filters");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "a", "category": "wildlife", "source": "manual", "tags": ["island"] })
            .to_string(),
    );
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "b", "category": "general" }).to_string(),
    );

    let by_category = authed_get(&server, "/api/memories?category=wildlife");
    assert_eq!(by_category.json()["total"], 1);

    let by_tag = authed_get(&server, "/api/memories?tags=island");
    assert_eq!(by_tag.json()["total"], 1);
    assert_eq!(by_tag.json()["memories"][0]["content"], "a");

    let by_source = authed_get(&server, "/api/memories?source=manual");
    assert_eq!(by_source.json()["total"], 2);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn list_pagination_reports_has_more() {
    let (server, root) = authed_server("list-paging");
    for i in 0..3 {
        add(&server, &format!("memory {}", i));
    }

    let response = authed_get(&server, "/api/memories?limit=2&offset=0");
    let body = response.json();
    assert_eq!(body["count"], 2);
    assert_eq!(body["has_more"], true);

    let last = authed_get(&server, "/api/memories?limit=2&offset=2");
    assert_eq!(last.json()["count"], 1);
    assert_eq!(last.json()["has_more"], false);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_invalid_limit_is_a_400_not_a_crash() {
    let (server, root) = authed_server("list-bad-limit");
    let response = authed_get(&server, "/api/memories?limit=notanumber");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_zero_limit_is_a_400_not_one_row() {
    let (server, root) = authed_server("list-zero-limit");
    add(&server, "first");
    add(&server, "second");
    let response = authed_get(&server, "/api/memories?limit=0");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// POST /api/memories
// ---------------------------------------------------------------------------

#[test]
fn adding_a_memory_returns_201_and_the_full_row() {
    let (server, root) = authed_server("add");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "a fact worth keeping", "tags": ["x"] }).to_string(),
    );
    assert_eq!(response.status, 201);
    let body = response.json();
    assert_eq!(body["content"], "a fact worth keeping");
    assert!(body["id"].as_str().unwrap().starts_with("mem_"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn missing_content_is_refused() {
    let (server, root) = authed_server("add-no-content");
    let response = authed_json(&server, "POST", "/api/memories", &json!({}).to_string());
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("content"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn blank_content_is_refused() {
    let (server, root) = authed_server("add-blank-content");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "   " }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn malformed_json_is_a_400() {
    let (server, root) = authed_server("add-malformed");
    let response = authed_json(&server, "POST", "/api/memories", "{not json");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

fn post_add(server: &ApiServer, body: serde_json::Value) -> common::Response {
    authed_json(server, "POST", "/api/memories", &body.to_string())
}

#[test]
fn project_and_memory_type_are_stored() {
    let (server, root) = authed_server("add-project-kind");
    let response = post_add(
        &server,
        json!({ "content": "a lesson", "project": "bbp_factory", "memory_type": "learning" }),
    );
    assert_eq!(response.status, 201);
    let id = response.json()["id"].as_str().unwrap().to_string();
    let stored = authed_get(&server, &format!("/api/memories/{id}")).json();
    assert_eq!(stored["project"], "bbp_factory");
    assert_eq!(stored["memory_type"], "learning");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn omitting_project_and_memory_type_behaves_as_before() {
    let (server, root) = authed_server("add-no-project-kind");
    let plain = post_add(&server, json!({ "content": "plain" })).json();
    let nulls = post_add(
        &server,
        json!({ "content": "nulls", "project": null, "memory_type": null }),
    )
    .json();
    assert_eq!(plain["memory_type"], nulls["memory_type"]);
    assert_eq!(plain["project"], nulls["project"]);
    assert_ne!(plain["memory_type"], "learning");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_blank_project_is_unset() {
    let (server, root) = authed_server("add-blank-project");
    let plain = post_add(&server, json!({ "content": "plain" })).json();
    let blank = post_add(&server, json!({ "content": "blank", "project": "  " })).json();
    assert_eq!(blank["project"], plain["project"]);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_memory_type_is_a_400() {
    let (server, root) = authed_server("add-bad-kind");
    let response = post_add(&server, json!({ "content": "x", "memory_type": "rumour" }));
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("rumour"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_decision_without_rationale_is_a_400() {
    let (server, root) = authed_server("add-decision-no-why");
    let refused = post_add(
        &server,
        json!({ "content": "x", "memory_type": "decision" }),
    );
    assert_eq!(refused.status, 400);
    assert!(refused.json()["error"]
        .as_str()
        .unwrap()
        .contains("rationale"));
    let accepted = post_add(
        &server,
        json!({ "content": "x", "memory_type": "decision", "metadata": { "rationale": "why" } }),
    );
    assert_eq!(accepted.status, 201);
    assert_eq!(accepted.json()["memory_type"], "decision");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn non_string_project_or_memory_type_is_a_400() {
    let (server, root) = authed_server("add-non-string");
    for body in [
        json!({ "content": "x", "project": 7 }),
        json!({ "content": "x", "memory_type": ["fact"] }),
    ] {
        let response = post_add(&server, body);
        assert_eq!(response.status, 400);
        assert!(response.json()["error"]
            .as_str()
            .unwrap()
            .contains("must be a string"));
    }
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/memories/{id}
// ---------------------------------------------------------------------------

#[test]
fn getting_a_known_memory_returns_it() {
    let (server, root) = authed_server("get-known");
    let id = add(&server, "findable");
    let response = authed_get(&server, &format!("/api/memories/{}", id));
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["content"], "findable");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn getting_an_unknown_memory_is_404() {
    let (server, root) = authed_server("get-unknown");
    let response = authed_get(&server, "/api/memories/mem_ghost");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// PUT/PATCH /api/memories/{id}
// ---------------------------------------------------------------------------

#[test]
fn updating_a_memory_changes_only_the_given_fields() {
    let (server, root) = authed_server("update");
    let id = add(&server, "original");

    let response = authed_json(
        &server,
        "PUT",
        &format!("/api/memories/{}", id),
        &json!({ "content": "revised" }).to_string(),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["content"], "revised");

    let patched = authed_json(
        &server,
        "PATCH",
        &format!("/api/memories/{}", id),
        &json!({ "tags": ["new"] }).to_string(),
    );
    assert_eq!(patched.status, 200);
    assert_eq!(
        patched.json()["content"],
        "revised",
        "PATCH left content alone"
    );
    assert_eq!(patched.json()["tags"][0], "new");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn updating_an_unknown_memory_is_404() {
    let (server, root) = authed_server("update-unknown");
    let response = authed_json(
        &server,
        "PUT",
        "/api/memories/mem_ghost",
        &json!({ "content": "x" }).to_string(),
    );
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn updating_with_no_fields_is_a_400() {
    let (server, root) = authed_server("update-no-fields");
    let id = add(&server, "original");
    let response = authed_json(&server, "PUT", &format!("/api/memories/{}", id), "{}");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// DELETE /api/memories/{id}
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_known_memory_removes_it() {
    let (server, root) = authed_server("delete");
    let id = add(&server, "doomed");

    let response = authed_json(&server, "DELETE", &format!("/api/memories/{}", id), "");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["deleted"], id);

    assert_eq!(
        authed_get(&server, &format!("/api/memories/{}", id)).status,
        404
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_an_unknown_memory_is_404() {
    let (server, root) = authed_server("delete-unknown");
    let response = authed_json(&server, "DELETE", "/api/memories/mem_ghost", "");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}
