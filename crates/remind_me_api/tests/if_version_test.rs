//! `if_version` over REST (card #1487, pack D120-D121): reads carry
//! `version`, a stale one answers 409 with the same body the MCP tools give.

mod common;
use common::{authed_get, authed_json, authed_server};
use serde_json::json;

#[test]
fn a_memory_update_with_a_stale_version_is_409_and_writes_nothing() {
    let (server, root) = authed_server("if-version-memory");
    let added = authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({"content": "fridays"}).to_string(),
    );
    let id = added.json()["id"].as_str().unwrap().to_string();
    let path = format!("/api/memories/{id}");
    let read = authed_get(&server, &path).json()["version"]
        .as_str()
        .unwrap()
        .to_string();

    let ok = authed_json(
        &server,
        "PATCH",
        &path,
        &json!({"content": "thursdays", "if_version": read}).to_string(),
    );
    assert_eq!(ok.status, 200);
    let now = ok.json()["version"].as_str().unwrap().to_string();
    assert_ne!(now, read);

    let refused = authed_json(
        &server,
        "PATCH",
        &path,
        &json!({"content": "mondays", "if_version": read}).to_string(),
    );
    assert_eq!(refused.status, 409);
    let body = refused.json();
    assert_eq!(body["error"]["code"], "version_conflict");
    assert_eq!(body["current_version"], now.as_str());
    assert!(body["updated_at"].as_str().is_some());
    assert_eq!(authed_get(&server, &path).json()["content"], "thursdays");

    let bad = authed_json(
        &server,
        "PATCH",
        &path,
        &json!({"content": "x", "if_version": "nope"}).to_string(),
    );
    assert_eq!(bad.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_wiki_write_with_a_stale_version_is_409() {
    let (server, root) = authed_server("if-version-wiki");
    let created = authed_json(
        &server,
        "POST",
        "/api/wiki",
        &json!({"title": "Deploys", "content": "Fridays.", "if_version": "absent"}).to_string(),
    );
    assert_eq!(created.status, 200, "{:?}", created.json());
    let version = created.json()["version"].as_str().unwrap().to_string();
    assert_eq!(
        authed_get(&server, "/api/wiki/deploys").json()["version"],
        version.as_str()
    );

    let refused = authed_json(
        &server,
        "POST",
        "/api/wiki",
        &json!({"title": "Deploys", "content": "Mondays.", "if_version": "absent"}).to_string(),
    );
    assert_eq!(refused.status, 409);
    assert_eq!(refused.json()["current_version"], version.as_str());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_project_set_with_a_stale_version_is_409() {
    let (server, root) = authed_server("if-version-project");
    let empty = authed_get(&server, "/api/projects?action=get&project=alpha").json()["version"]
        .as_str()
        .unwrap()
        .to_string();
    let set = json!({"action": "set", "project": "alpha", "stage": "idea", "status": "active",
                     "because": "started", "if_version": empty});
    assert_eq!(
        authed_json(&server, "POST", "/api/projects", &set.to_string()).status,
        200
    );
    let refused = authed_json(&server, "POST", "/api/projects", &set.to_string());
    assert_eq!(refused.status, 409);
    assert_eq!(refused.json()["error"]["code"], "version_conflict");
    std::fs::remove_dir_all(&root).unwrap();
}
