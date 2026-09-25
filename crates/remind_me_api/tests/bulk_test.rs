//! Coverage for `POST /api/memories/bulk/{delete,tag,reclassify}` — the
//! HTTP-only routes with no MCP-tool equivalent.

mod common;
use common::{authed_get, authed_json, authed_server};
use remind_me_api::ApiServer;
use serde_json::json;

fn add(server: &ApiServer, content: &str) -> String {
    authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content }).to_string(),
    )
    .json()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// bulk/delete
// ---------------------------------------------------------------------------

#[test]
fn bulk_delete_removes_every_id() {
    let (server, root) = authed_server("bulk-delete");
    let a = add(&server, "alpha");
    let b = add(&server, "beta");

    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/delete",
        &json!({ "ids": [a, b] }).to_string(),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["deleted"].as_array().unwrap().len(), 2);
    assert_eq!(authed_get(&server, "/api/memories").json()["total"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_missing_id_is_reported_without_failing_the_batch() {
    let (server, root) = authed_server("bulk-delete-missing");
    let a = add(&server, "alpha");

    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/delete",
        &json!({ "ids": [a, "mem_ghost"] }).to_string(),
    );
    let body = response.json();
    assert_eq!(body["not_found"], json!(["mem_ghost"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_id_list_is_a_400() {
    let (server, root) = authed_server("bulk-delete-empty");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/delete",
        &json!({ "ids": [] }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn more_than_200_ids_is_a_400() {
    let (server, root) = authed_server("bulk-delete-too-many");
    let ids: Vec<String> = (0..201).map(|i| format!("mem_{}", i)).collect();
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/delete",
        &json!({ "ids": ids }).to_string(),
    );
    assert_eq!(response.status, 400);
    assert!(response.json()["error"].as_str().unwrap().contains("200"));
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// bulk/tag
// ---------------------------------------------------------------------------

#[test]
fn bulk_tag_defaults_to_add_mode() {
    let (server, root) = authed_server("bulk-tag-add");
    let a = add(&server, "alpha");

    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a.clone()], "tags": ["new"] }).to_string(),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["updated"], json!([a.clone()]));

    let memory = authed_get(&server, &format!("/api/memories/{}", a));
    assert_eq!(memory.json()["tags"], json!(["new"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn bulk_tag_set_mode_replaces_tags() {
    let (server, root) = authed_server("bulk-tag-set");
    let a = add(&server, "alpha");
    authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a.clone()], "tags": ["first"] }).to_string(),
    );

    authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a.clone()], "tags": ["second"], "mode": "set" }).to_string(),
    );

    let memory = authed_get(&server, &format!("/api/memories/{}", a));
    assert_eq!(memory.json()["tags"], json!(["second"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn bulk_tag_remove_mode_drops_named_tags() {
    let (server, root) = authed_server("bulk-tag-remove");
    let a = add(&server, "alpha");
    authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a.clone()], "tags": ["keep", "drop"] }).to_string(),
    );

    authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a.clone()], "tags": ["drop"], "mode": "remove" }).to_string(),
    );

    let memory = authed_get(&server, &format!("/api/memories/{}", a));
    assert_eq!(memory.json()["tags"], json!(["keep"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_invalid_mode_is_a_400() {
    let (server, root) = authed_server("bulk-tag-bad-mode");
    let a = add(&server, "alpha");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a], "tags": ["x"], "mode": "sideways" }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn missing_tags_is_a_400() {
    let (server, root) = authed_server("bulk-tag-no-tags");
    let a = add(&server, "alpha");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/tag",
        &json!({ "ids": [a] }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// bulk/reclassify
// ---------------------------------------------------------------------------

#[test]
fn bulk_reclassify_updates_memory_type_and_decay_rate() {
    let (server, root) = authed_server("bulk-reclassify");
    let a = add(&server, "a decision was made");

    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/reclassify",
        &json!({ "classifications": [{ "memory_id": a, "memory_type": "decision" }] }).to_string(),
    );
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["updated"], 1);
    assert_eq!(body["total"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn bulk_reclassify_reports_a_missing_id_without_failing_the_batch() {
    let (server, root) = authed_server("bulk-reclassify-missing");
    let a = add(&server, "a decision");

    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/reclassify",
        &json!({ "classifications": [
            { "memory_id": a, "memory_type": "decision" },
            { "memory_id": "mem_ghost", "memory_type": "fact" }
        ] })
        .to_string(),
    );
    let body = response.json();
    assert_eq!(body["updated"], 1);
    assert_eq!(body["not_found"], json!(["mem_ghost"]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_classifications_list_is_a_400() {
    let (server, root) = authed_server("bulk-reclassify-empty");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/reclassify",
        &json!({ "classifications": [] }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_malformed_classification_entry_is_a_400() {
    let (server, root) = authed_server("bulk-reclassify-malformed");
    let response = authed_json(
        &server,
        "POST",
        "/api/memories/bulk/reclassify",
        &json!({ "classifications": [{ "memory_id": "" , "memory_type": "decision" }] })
            .to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}
