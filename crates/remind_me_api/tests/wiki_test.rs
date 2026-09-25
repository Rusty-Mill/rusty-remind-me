//! Coverage for the read-only wiki routes: `GET /api/wiki`,
//! `/api/wiki/search`, `/api/wiki/load`, `/api/wiki/status`,
//! `/api/wiki/{slug}`.
//!
//! There is deliberately no POST/PUT/DELETE here — the wiki is LLM-curated
//! by design, and only the MCP tools write it. Pages are seeded via
//! `Wiki::write_page` directly, through `common::seeded_wiki_server`.

mod common;
use common::{get, seeded_wiki_server, server};

// ---------------------------------------------------------------------------
// GET /api/wiki
// ---------------------------------------------------------------------------

#[test]
fn an_empty_wiki_lists_no_pages() {
    let (server, root) = server("wiki-empty");
    let response = get(&server, "/api/wiki");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_written_page_is_listed() {
    let (server, root) = seeded_wiki_server("wiki-listed", |conn, wiki| {
        wiki.write_page(conn, "VLAN Setup", "how the network is laid out", None)
            .unwrap()
            .unwrap();
    });

    let response = get(&server, "/api/wiki");
    let body = response.json();
    assert_eq!(body["count"], 1);
    assert_eq!(body["pages"][0]["title"], "VLAN Setup");
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/wiki/{slug}
// ---------------------------------------------------------------------------

#[test]
fn a_known_page_is_read_by_slug_or_title() {
    let (server, root) = seeded_wiki_server("wiki-read", |conn, wiki| {
        wiki.write_page(conn, "VLAN Setup", "how the network is laid out", None)
            .unwrap()
            .unwrap();
    });

    let by_slug = get(&server, "/api/wiki/vlan-setup");
    assert_eq!(by_slug.status, 200);
    assert!(by_slug.json()["content"]
        .as_str()
        .unwrap()
        .contains("how the network is laid out"));

    let by_title = get(&server, "/api/wiki/VLAN%20Setup");
    assert_eq!(by_title.status, 200);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_page_is_404() {
    let (server, root) = server("wiki-unknown-page");
    let response = get(&server, "/api/wiki/nowhere");
    assert_eq!(response.status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/wiki/search
// ---------------------------------------------------------------------------

#[test]
fn wiki_search_requires_q() {
    let (server, root) = server("wiki-search-no-q");
    let response = get(&server, "/api/wiki/search");
    assert_eq!(response.status, 400);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wiki_search_finds_a_matching_page() {
    let (server, root) = seeded_wiki_server("wiki-search-hit", |conn, wiki| {
        wiki.write_page(conn, "VLAN Setup", "the switch uses trunk ports", None)
            .unwrap()
            .unwrap();
        wiki.write_page(conn, "Unrelated", "nothing to do with networking", None)
            .unwrap()
            .unwrap();
    });

    let response = get(&server, "/api/wiki/search?q=trunk");
    let body = response.json();
    assert_eq!(body["count"], 1);
    assert_eq!(body["results"][0]["title"], "VLAN Setup");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wiki_search_finds_nothing_in_an_empty_wiki() {
    let (server, root) = server("wiki-search-empty");
    let response = get(&server, "/api/wiki/search?q=anything");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["count"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/wiki/load
// ---------------------------------------------------------------------------

#[test]
fn loading_an_empty_wiki_returns_nothing_included() {
    let (server, root) = server("wiki-load-empty");
    let response = get(&server, "/api/wiki/load");
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["pages_included"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn loading_a_populated_wiki_concatenates_every_page() {
    let (server, root) = seeded_wiki_server("wiki-load-populated", |conn, wiki| {
        wiki.write_page(conn, "First", "first body", None)
            .unwrap()
            .unwrap();
        wiki.write_page(conn, "Second", "second body", None)
            .unwrap()
            .unwrap();
    });

    let response = get(&server, "/api/wiki/load");
    let body = response.json();
    assert_eq!(body["pages_included"], 2);
    assert!(body["content"].as_str().unwrap().contains("first body"));
    assert!(body["content"].as_str().unwrap().contains("second body"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_token_budget_of_zero_means_unlimited_matching_the_mcp_tools_default() {
    let (server, root) = seeded_wiki_server("wiki-load-default-budget", |conn, wiki| {
        wiki.write_page(conn, "Only Page", &"word ".repeat(2000), None)
            .unwrap()
            .unwrap();
    });

    // No token_budget query param at all — the default must not truncate.
    let response = get(&server, "/api/wiki/load");
    assert_eq!(response.json()["pages_omitted"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn include_index_false_omits_the_catalogue() {
    let (server, root) = seeded_wiki_server("wiki-load-no-index", |conn, wiki| {
        wiki.write_page(conn, "First", "first body", None)
            .unwrap()
            .unwrap();
    });

    let response = get(&server, "/api/wiki/load?include_index=false");
    assert!(!response.json()["content"]
        .as_str()
        .unwrap()
        .contains("# Wiki Index"));
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/wiki/status
// ---------------------------------------------------------------------------

#[test]
fn wiki_status_reports_pages_and_pending_compile() {
    let (server, root) = seeded_wiki_server("wiki-status", |conn, wiki| {
        wiki.write_page(conn, "First", "first body", None)
            .unwrap()
            .unwrap();
        // A raw memory not yet synthesised into the wiki.
        remind_me_core::db::queries::add_memory(
            conn,
            remind_me_core::MemoryAddInput {
                sensitive: false,
                content: "a fact awaiting synthesis".into(),
                category: "general".into(),
                tags: vec![],
                source: "manual".into(),
                metadata: serde_json::json!({}),
                subject: None,
                predicate: None,
                object: None,
                entities: vec![],
            },
        )
        .unwrap();
    });

    let response = get(&server, "/api/wiki/status");
    let body = response.json();
    assert_eq!(body["pages"], 1);
    assert_eq!(body["pending_compile"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// Routing: literal sub-paths are not swallowed by {slug}
// ---------------------------------------------------------------------------

#[test]
fn search_load_and_status_do_not_resolve_as_page_slugs() {
    let (server, root) = server("wiki-routing");
    // Each of these has its own dedicated handler; if `{slug}` matched first
    // they would 404 as "page not found: search" etc. instead of running
    // their real route (search/load 400 on missing params they don't have
    // here is fine — status has none and returns 200).
    assert_eq!(get(&server, "/api/wiki/search").status, 400);
    assert_eq!(get(&server, "/api/wiki/load").status, 200);
    assert_eq!(get(&server, "/api/wiki/status").status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
