//! Coverage for the wiki write routes: `POST /api/wiki`,
//! `DELETE /api/wiki/{slug}`, `POST /api/wiki/compile`, `GET /api/wiki/schema`.
//!
//! `wiki_test.rs` covers the read paths, which predate these and stay
//! unauthenticated-readable. Everything here either writes a file under the
//! wiki root or advances the compile watermark, so the auth posture and the
//! reserved-page refusals are as much the subject as the happy path is.

mod common;
use common::{authed_get, authed_json, authed_server, call, get, server, unauthed_json, KEY};
use remind_me_api::ApiServer;
use serde_json::json;

fn write(server: &ApiServer, body: serde_json::Value) -> common::Response {
    authed_json(server, "POST", "/api/wiki", &body.to_string())
}

fn delete(server: &ApiServer, slug: &str) -> common::Response {
    call(
        server,
        "DELETE",
        &format!("/api/wiki/{}", slug),
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        "",
    )
}

fn compile(server: &ApiServer, body: serde_json::Value) -> common::Response {
    authed_json(server, "POST", "/api/wiki/compile", &body.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/wiki
// ---------------------------------------------------------------------------

#[test]
fn a_page_is_created_and_then_updated_in_place() {
    let (server, root) = authed_server("wiki-write-create");

    let created = write(
        &server,
        json!({ "title": "VLAN Setup", "content": "how the network is laid out" }),
    );
    assert_eq!(created.status, 200);
    let body = created.json();
    assert_eq!(body["slug"], "vlan-setup");
    assert_eq!(body["title"], "VLAN Setup");
    assert_eq!(body["created"], true);

    // The same title writes the same slug, replacing the body wholesale.
    let updated = write(
        &server,
        json!({ "title": "VLAN Setup", "content": "guest devices are on VLAN 30" }),
    );
    assert_eq!(updated.json()["created"], false);

    let page = authed_get(&server, "/api/wiki/vlan-setup").json();
    let content = page["content"].as_str().unwrap();
    assert!(content.contains("guest devices are on VLAN 30"));
    assert!(
        !content.contains("how the network is laid out"),
        "content is replaced, not appended, got {:?}",
        content
    );

    // And exactly one page exists, not two.
    assert_eq!(authed_get(&server, "/api/wiki").json()["count"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wikilinks_in_the_body_are_reported_back() {
    let (server, root) = authed_server("wiki-write-links");
    let body = write(
        &server,
        json!({ "title": "Network", "content": "see [[VLAN Setup]] and [[Firewall]]" }),
    )
    .json();
    let links: Vec<String> = body["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(links.contains(&"VLAN Setup".to_string()));
    assert!(links.contains(&"Firewall".to_string()));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_log_note_is_accepted_alongside_the_write() {
    let (server, root) = authed_server("wiki-write-note");
    let response = write(
        &server,
        json!({ "title": "Network", "content": "body", "log_note": "split out of the old page" }),
    );
    assert_eq!(response.status, 200);
    let log = std::fs::read_to_string(root.join("log.md")).unwrap();
    assert!(
        log.contains("split out of the old page"),
        "the note reaches log.md, got {:?}",
        log
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_missing_title_or_content_is_400() {
    let (server, root) = authed_server("wiki-write-invalid");
    assert_eq!(write(&server, json!({ "content": "body" })).status, 400);
    assert_eq!(write(&server, json!({ "title": "Network" })).status, 400);
    assert_eq!(
        write(&server, json!({ "title": "  ", "content": "body" })).status,
        400
    );
    assert_eq!(
        write(&server, json!({ "title": "Network", "content": "   " })).status,
        400
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn over_length_fields_are_refused_at_the_same_bounds_the_mcp_tool_uses() {
    // Core enforces none of these; the tool's JSON schema does. A page
    // writable here but rejected over MCP would be a page the two surfaces
    // disagree about.
    let (server, root) = authed_server("wiki-write-limits");

    let long_title = "t".repeat(201);
    assert_eq!(
        write(&server, json!({ "title": long_title, "content": "body" })).status,
        400
    );

    let long_body = "c".repeat(100_001);
    assert_eq!(
        write(&server, json!({ "title": "Network", "content": long_body })).status,
        400
    );

    let long_note = "n".repeat(501);
    assert_eq!(
        write(
            &server,
            json!({ "title": "Network", "content": "body", "log_note": long_note })
        )
        .status,
        400
    );

    // The bounds are inclusive: exactly at the limit is fine.
    assert_eq!(
        write(
            &server,
            json!({ "title": "t".repeat(200), "content": "body" })
        )
        .status,
        200
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_reserved_system_page_cannot_be_written() {
    // index.md, log.md and schema.md are generated. A hand-written index would
    // put the index permanently at odds with the pages it claims to list.
    let (server, root) = authed_server("wiki-write-reserved");
    for title in ["index", "Index", "log", "schema"] {
        let response = write(&server, json!({ "title": title, "content": "hijacked" }));
        assert_eq!(
            response.status, 403,
            "{:?} should be refused, got {}",
            title, response.body
        );
        assert!(response.json()["error"]
            .as_str()
            .unwrap()
            .contains("reserved"));
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_title_cannot_escape_the_wiki_root_however_it_is_spelled() {
    // Containment is structural: `slugify` keeps ASCII alphanumerics and turns
    // everything else into a dash, so a separator, a `..` or an extension
    // cannot survive into the filename. Asserted rather than assumed, because
    // this is the property that makes a writable wiki safe to expose at all.
    let (server, root) = authed_server("wiki-write-traversal");

    for title in [
        "../../etc/passwd",
        "..",
        "/etc/shadow",
        "a/../../b",
        ".bashrc",
        "page.md.md",
        "C:\\Windows\\System32",
    ] {
        let response = write(&server, json!({ "title": title, "content": "body" }));
        assert_eq!(
            response.status, 200,
            "{:?} should be slugified, not refused, got {}",
            title, response.body
        );
        let path = response.json()["path"].as_str().unwrap().to_string();
        let resolved = std::path::Path::new(&path);
        assert_eq!(
            resolved.parent().unwrap(),
            root,
            "{:?} wrote outside the wiki root: {}",
            title,
            path
        );
        assert_eq!(
            resolved.extension().and_then(|e| e.to_str()),
            Some("md"),
            "{:?} wrote a non-markdown file: {}",
            title,
            path
        );
    }

    // Nothing landed anywhere but the root, and nothing landed in it that is
    // not a .md file.
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        assert!(
            path.is_file(),
            "unexpected directory in the wiki root: {:?}",
            path
        );
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn writing_a_page_is_refused_unauthenticated() {
    let (server, root) = server("wiki-write-unauthed");
    let response = unauthed_json(
        &server,
        "POST",
        "/api/wiki",
        &json!({ "title": "Network", "content": "body" }).to_string(),
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn reading_the_wiki_is_still_open_when_no_key_is_configured() {
    // The read routes predate the write ones and must not have been dragged
    // behind the key by adding them.
    let (server, root) = server("wiki-read-still-open");
    assert_eq!(get(&server, "/api/wiki").status, 200);
    assert_eq!(get(&server, "/api/wiki/status").status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// DELETE /api/wiki/{slug}
// ---------------------------------------------------------------------------

#[test]
fn a_page_is_deleted_by_slug_and_its_file_goes_with_it() {
    let (server, root) = authed_server("wiki-delete");
    write(
        &server,
        json!({ "title": "VLAN Setup", "content": "how the network is laid out" }),
    );
    assert!(root.join("vlan-setup.md").exists());

    let response = delete(&server, "vlan-setup");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["deleted"], "vlan-setup");
    assert!(!root.join("vlan-setup.md").exists());
    assert_eq!(authed_get(&server, "/api/wiki").json()["count"], 0);
    assert_eq!(authed_get(&server, "/api/wiki/vlan-setup").status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_page_can_be_deleted_by_title_too() {
    let (server, root) = authed_server("wiki-delete-by-title");
    write(&server, json!({ "title": "VLAN Setup", "content": "body" }));
    assert_eq!(delete(&server, "VLAN%20Setup").status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_an_unknown_page_is_404() {
    let (server, root) = authed_server("wiki-delete-unknown");
    assert_eq!(delete(&server, "nowhere").status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_reserved_system_page_cannot_be_deleted() {
    let (server, root) = authed_server("wiki-delete-reserved");
    let response = delete(&server, "index");
    assert_eq!(response.status, 403);
    assert!(response.json()["error"]
        .as_str()
        .unwrap()
        .contains("reserved"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_a_page_is_refused_unauthenticated() {
    let (server, root) = server("wiki-delete-unauthed");
    let response = call(
        &server,
        "DELETE",
        "/api/wiki/anything",
        None,
        Some("application/json"),
        "",
    );
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// POST /api/wiki/compile
// ---------------------------------------------------------------------------

fn add_memory(server: &ApiServer, content: &str) {
    authed_json(
        server,
        "POST",
        "/api/memories",
        &json!({ "content": content }).to_string(),
    );
}

#[test]
fn a_compile_brief_lists_pending_memories_without_advancing_the_watermark() {
    let (server, root) = authed_server("wiki-compile-brief");
    add_memory(&server, "the boiler service is due in the spring");

    let first = compile(&server, json!({}));
    assert_eq!(first.status, 200);
    let body = first.json();
    assert_eq!(body["status"], "brief");
    assert_eq!(body["pending"], 1);
    assert!(body["brief"]
        .as_str()
        .unwrap()
        .contains("the boiler service is due in the spring"));

    // Safe to call repeatedly: the same brief comes back, because phase one
    // moves nothing.
    let second = compile(&server, json!({}));
    assert_eq!(second.json()["pending"], 1);
    assert_eq!(
        authed_get(&server, "/api/wiki/status").json()["pending_compile"],
        1
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn marking_integrated_advances_the_watermark_and_clears_the_pending_count() {
    let (server, root) = authed_server("wiki-compile-integrate");
    add_memory(&server, "the boiler service is due in the spring");

    let integrated = compile(&server, json!({ "mark_integrated": true }));
    assert_eq!(integrated.status, 200);
    let body = integrated.json();
    assert_eq!(body["status"], "integrated");
    assert_eq!(body["sources_marked"], 1);

    assert_eq!(
        authed_get(&server, "/api/wiki/status").json()["pending_compile"],
        0
    );
    assert_eq!(compile(&server, json!({})).json()["status"], "noop");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_empty_vault_compiles_to_a_noop_rather_than_an_empty_brief() {
    let (server, root) = authed_server("wiki-compile-noop");
    let body = compile(&server, json!({})).json();
    assert_eq!(body["status"], "noop");
    assert!(body["reason"].as_str().unwrap().contains("nothing pending"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_absent_body_is_treated_as_brief_me_with_the_defaults() {
    // The common call. An empty body is not malformed, unlike garbage.
    let (server, root) = authed_server("wiki-compile-no-body");
    add_memory(&server, "something to synthesise");
    let response = call(
        &server,
        "POST",
        "/api/wiki/compile",
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        "",
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["status"], "brief");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_non_numeric_limit_or_non_boolean_flag_is_400() {
    let (server, root) = authed_server("wiki-compile-bad-args");
    assert_eq!(compile(&server, json!({ "limit": "lots" })).status, 400);
    assert_eq!(
        compile(&server, json!({ "mark_integrated": "yes" })).status,
        400
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_limit_bounds_how_many_sources_a_brief_carries() {
    let (server, root) = authed_server("wiki-compile-limit");
    for i in 0..5 {
        add_memory(&server, &format!("memory number {}", i));
    }
    let body = compile(&server, json!({ "limit": 2 })).json();
    assert_eq!(body["pending"], 2, "the brief is capped, got {}", body);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn compiling_is_refused_unauthenticated() {
    let (server, root) = server("wiki-compile-unauthed");
    let response = unauthed_json(&server, "POST", "/api/wiki/compile", "{}");
    assert_eq!(response.status, 401);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/wiki/schema
// ---------------------------------------------------------------------------

#[test]
fn the_maintainer_schema_is_readable_and_seeded_on_first_read() {
    let (server, root) = authed_server("wiki-schema");
    assert!(!root.join("schema.md").exists());

    let response = authed_get(&server, "/api/wiki/schema");
    assert_eq!(response.status, 200);
    assert!(!response.json()["schema"]
        .as_str()
        .unwrap()
        .trim()
        .is_empty());
    assert!(
        root.join("schema.md").exists(),
        "reading the schema seeds the default"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_page_slugged_schema_does_not_shadow_the_schema_route() {
    // Both patterns are four segments. The literal is registered first, so the
    // route wins for GET -- which is also why `schema` is a reserved slug and
    // no page can be written at it in the first place.
    let (server, root) = authed_server("wiki-schema-shadow");
    assert_eq!(
        write(&server, json!({ "title": "Schema", "content": "body" })).status,
        403
    );
    assert!(authed_get(&server, "/api/wiki/schema").json()["schema"].is_string());
    std::fs::remove_dir_all(&root).unwrap();
}
