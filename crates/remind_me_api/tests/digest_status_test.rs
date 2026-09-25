//! Coverage for `GET /api/digest` and `GET /api/status` — the read-only
//! commands the dashboard renders as panels.

mod common;
use common::{authed_get, authed_json, authed_server};
use remind_me_api::ApiServer;
use serde_json::json;

fn add(server: &ApiServer, content: &str, sensitive: bool) -> String {
    let response = authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content, "sensitive": sensitive }).to_string(),
    );
    response.json()["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// GET /api/digest
// ---------------------------------------------------------------------------

#[test]
fn a_digest_reports_recent_memories_and_vitality() {
    let (server, root) = authed_server("digest-basic");
    add(&server, "the boiler needs a service", false);

    let response = authed_get(&server, "/api/digest");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["since_days"], 7);
    assert_eq!(body["recent_total"], 1);
    assert_eq!(
        body["recent_memories"][0]["content"],
        "the boiler needs a service"
    );
    assert!(body["vitality"].is_object());
    assert!(body["reminders_upcoming"].is_array());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_digest_never_includes_sensitive_memories() {
    // Core excludes them with no override, and this route deliberately adds
    // none — a dashboard panel is more ambient than a tool call, not less.
    let (server, root) = authed_server("digest-sensitive");
    add(&server, "public note", false);
    add(&server, "the safe combination", true);

    let body = authed_get(&server, "/api/digest").json();
    assert_eq!(body["recent_total"], 1);
    let rendered = body["recent_memories"].to_string();
    assert!(
        !rendered.contains("safe combination"),
        "a sensitive memory leaked into the digest: {}",
        rendered
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn since_days_is_clamped_rather_than_refused() {
    let (server, root) = authed_server("digest-clamp");
    assert_eq!(
        authed_get(&server, "/api/digest?since_days=0").json()["since_days"],
        1
    );
    assert_eq!(
        authed_get(&server, "/api/digest?since_days=99999").json()["since_days"],
        365
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_non_numeric_since_days_is_400() {
    let (server, root) = authed_server("digest-bad-since");
    let response = authed_get(&server, "/api/digest?since_days=lots");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/status
// ---------------------------------------------------------------------------

#[test]
fn status_reports_the_build_schema_and_memory_count() {
    let (server, root) = authed_server("status-basic");
    add(&server, "one", false);
    add(&server, "two", false);

    let response = authed_get(&server, "/api/status");
    assert_eq!(response.status, 200);
    let body = response.json();
    assert_eq!(body["memory_count"], 2);
    assert_eq!(body["version"], remind_me_core::updater::INSTALLED_VERSION);
    assert_eq!(body["schema_current"], true);
    assert_eq!(
        body["schema_version"], body["expected_schema_version"],
        "a freshly-migrated store is on the version this build expects"
    );
    // Subsystems are reported as a tagged state, not as a bare bool.
    assert!(body["mcp"]["state"].is_string());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_dashboard_subsystem_is_active_because_this_process_is_serving_it() {
    // Core reports `dashboard` as not-implemented, because from inside an MCP
    // process the daemon is another process it can only find via a PID file.
    // Answering the same way from inside that daemon would be a report the
    // delivery of the report itself disproves.
    let (server, root) = authed_server("status-dashboard");
    let body = authed_get(&server, "/api/status").json();
    assert_eq!(body["dashboard"]["state"], "active");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_in_memory_store_omits_the_database_path_rather_than_inventing_one() {
    let (server, root) = authed_server("status-in-memory");
    let body = authed_get(&server, "/api/status").json();
    assert!(
        body.get("database_path").is_none(),
        "an in-memory database has no file, got {}",
        body
    );
    assert_eq!(body["database_exists"], true);
    std::fs::remove_dir_all(&root).unwrap();
}
