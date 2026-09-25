//! Coverage for `POST /api/import` and `GET /api/export`.

mod common;
use common::{authed_get, authed_json, authed_server};
use serde_json::json;

/// A scratch directory inside the default import/export root (the home
/// directory) — matching `remind_me_core`'s own import test convention.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_api_io_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// POST /api/import
// ---------------------------------------------------------------------------

#[test]
fn missing_file_path_is_a_400() {
    let (server, root) = authed_server("import-no-path");
    let response = authed_json(&server, "POST", "/api/import", &json!({}).to_string());
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_path_outside_the_import_roots_is_refused() {
    let (server, root) = authed_server("import-outside-roots");
    let response = authed_json(
        &server,
        "POST",
        "/api/import",
        &json!({ "file_path": "/etc/hosts" }).to_string(),
    );
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("import roots"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_missing_file_is_a_400() {
    let (server, root) = authed_server("import-missing-file");
    let dir = scratch("missing");
    let response = authed_json(
        &server,
        "POST",
        "/api/import",
        &json!({ "file_path": dir.join("nothing.md").display().to_string() }).to_string(),
    );
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_single_file_is_imported() {
    let (server, root) = authed_server("import-file");
    let dir = scratch("file");
    let path = dir.join("notes.md");
    std::fs::write(&path, "# Notes\n\nsomething worth keeping").unwrap();

    let response = authed_json(
        &server,
        "POST",
        "/api/import",
        &json!({ "file_path": path.display().to_string() }).to_string(),
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.json()["status"], "imported");

    let list = authed_get(&server, "/api/memories");
    assert_eq!(list.json()["total"], 1);

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_directory_is_imported_recursively() {
    let (server, root) = authed_server("import-dir");
    let dir = scratch("dir");
    std::fs::write(dir.join("a.md"), "# A\n\nfirst").unwrap();
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("b.md"), "# B\n\nsecond").unwrap();

    let response = authed_json(
        &server,
        "POST",
        "/api/import",
        &json!({ "file_path": dir.display().to_string() }).to_string(),
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.json()["files_seen"], 2);

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn importing_the_same_file_twice_is_a_no_op() {
    let (server, root) = authed_server("import-dedup");
    let dir = scratch("dedup");
    let path = dir.join("notes.md");
    std::fs::write(&path, "# Notes\n\nunchanging content").unwrap();
    let body = json!({ "file_path": path.display().to_string() }).to_string();

    authed_json(&server, "POST", "/api/import", &body);
    let second = authed_json(&server, "POST", "/api/import", &body);

    assert_eq!(second.json()["status"], "skipped");
    assert_eq!(authed_get(&server, "/api/memories").json()["total"], 1);

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unsupported_extension_is_refused() {
    let (server, root) = authed_server("import-bad-suffix");
    let dir = scratch("badsuffix");
    let path = dir.join("archive.pdf");
    std::fs::write(&path, "%PDF-1.7").unwrap();

    let response = authed_json(
        &server,
        "POST",
        "/api/import",
        &json!({ "file_path": path.display().to_string() }).to_string(),
    );
    assert_eq!(
        response.status, 200,
        "a well-formed request with unusable content, not a 400"
    );
    assert_eq!(response.json()["status"], "failed");

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/export
// ---------------------------------------------------------------------------

#[test]
fn an_invalid_format_is_a_400() {
    let (server, root) = authed_server("export-bad-format");
    let response = authed_get(&server, "/api/export?format=xml");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_store_exports_an_empty_array() {
    let (server, root) = authed_server("export-empty");
    let response = authed_get(&server, "/api/export");
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "application/json");
    let parsed: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn without_a_file_path_the_export_body_is_the_payload_itself() {
    let (server, root) = authed_server("export-inline");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "exportable" }).to_string(),
    );

    let response = authed_get(&server, "/api/export");
    assert_eq!(response.status, 200);
    // Not a JSON-wrapped summary — the export's own records, as the body.
    let records: Vec<serde_json::Value> = serde_json::from_str(&response.body).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"], "exportable");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn jsonl_format_uses_the_ndjson_content_type() {
    let (server, root) = authed_server("export-jsonl");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "a line" }).to_string(),
    );

    let response = authed_get(&server, "/api/export?format=jsonl");
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "application/x-ndjson");
    assert_eq!(response.body.trim().lines().count(), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn category_and_tags_filter_the_export() {
    let (server, root) = authed_server("export-filters");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "a", "category": "wildlife" }).to_string(),
    );
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "b", "category": "general" }).to_string(),
    );

    let response = authed_get(&server, "/api/export?category=wildlife");
    let records: Vec<serde_json::Value> = serde_json::from_str(&response.body).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"], "a");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_file_path_outside_the_export_roots_is_refused() {
    let (server, root) = authed_server("export-outside-roots");
    let response = authed_get(&server, "/api/export?file_path=/etc/exported.json");
    assert_eq!(response.status, 400);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("export roots"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_file_path_writes_to_disk_and_returns_a_json_summary() {
    let (server, root) = authed_server("export-to-file");
    authed_json(
        &server,
        "POST",
        "/api/memories",
        &json!({ "content": "exportable" }).to_string(),
    );
    let dir = scratch("export-dest");
    let path = dir.join("backup.json");

    let response = authed_get(
        &server,
        &format!("/api/export?file_path={}", path.display()),
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    let body = response.json();
    assert_eq!(body["exported"], 1);
    assert!(path.exists());

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
