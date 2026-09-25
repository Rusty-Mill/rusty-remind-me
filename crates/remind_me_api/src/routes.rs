//! Route handlers: one per HTTP endpoint, each a thin wrapper over an
//! existing `remind_me_core` function.
//!
//! Nothing here reimplements query logic that already exists for the MCP
//! tools or another HTTP route — see each handler's doc comment for which
//! core function it defers to. The `bulk/*` routes are the exception: they
//! have no MCP-tool equivalent (a dashboard selects a batch, then acts on
//! exactly that selection), so their logic lives in
//! `remind_me_core::db::queries::{bulk_delete, bulk_tag}` as new, tested core
//! functions rather than inline here.

use crate::http::{Body, Request};
use remind_me_core::db::queries;
use remind_me_core::entity::{entity_profile, list_entities, traverse_from_name};
use remind_me_core::import_paths::{self, ImportPathError};
use remind_me_core::status::SubsystemStatus;
use remind_me_core::webhook::constant_time_eq;
use remind_me_core::wiki_fs::{pending_compile_count, Wiki};
use remind_me_core::{
    self as core, digest, export, importer, saved_searches, stats, vitality, BulkImportDirInput,
    BulkTagInput, ChatImportInput, EntityTraverseInput, ExportFormat, ExportInput, MemoryAddInput,
    MemoryListInput, MemoryUpdateInput, ReclassifyInput, ReminderWindow, SaveSearchInput,
    SearchPageInput, SetReminderInput, SetReminderOutcome, UpdateOutcome, BULK_IDS_MAX,
    LIST_LIMIT_MAX, LIST_LIMIT_MIN, REMINDER_LIMIT_MAX, REMINDER_LIMIT_MIN,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

pub type Params = HashMap<String, String>;
pub type Handler = fn(&Connection, &Wiki, &Request, &Params) -> (u16, Body);

pub struct Route {
    pub methods: &'static [&'static str],
    pub pattern: &'static str,
    pub handler: Handler,
}

fn ok(value: impl serde::Serialize) -> (u16, Body) {
    (
        200,
        Body::Json(serde_json::to_value(value).unwrap_or(json!({}))),
    )
}

fn err(status: u16, message: impl Into<String>) -> (u16, Body) {
    (status, Body::Json(json!({ "error": message.into() })))
}

fn internal_err(e: impl std::fmt::Display) -> (u16, Body) {
    err(500, e.to_string())
}

/// Parse the request body as JSON, or `Err` with the 400 response to send.
fn json_body(request: &Request) -> Result<Value, (u16, Body)> {
    serde_json::from_slice(&request.body).map_err(|e| err(400, format!("Invalid JSON body: {}", e)))
}

/// Parse the request body into `T`, or `Err` with the 400 response to send.
fn parsed_body<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, (u16, Body)> {
    serde_json::from_slice(&request.body).map_err(|e| err(400, format!("Invalid JSON body: {}", e)))
}

fn int_query(request: &Request, name: &str, default: usize) -> Result<usize, (u16, Body)> {
    request.query_usize(name, default).map_err(|e| err(400, e))
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// Unauthenticated liveness probe. Reveals no data — always public, even when
/// `REMIND_ME_API_KEY` is set, matching the reference's own rationale: a
/// health check has to work whether or not auth is configured.
pub fn health(_conn: &Connection, _wiki: &Wiki, _req: &Request, _params: &Params) -> (u16, Body) {
    // The version rides on `/health` rather than only on `/api/versions`
    // because this route is unauthenticated, and the reference is explicit
    // about why that matters: a wrong or missing API key is exactly the
    // situation where you most want to know which build you are talking to.
    // The dashboard header reads its own node's version from here for that
    // reason, and asks `/api/versions` only for the hub's.
    ok(json!({
        "status": "ok",
        "version": remind_me_core::updater::INSTALLED_VERSION,
    }))
}

/// The recorded history of the vault's shape, oldest first.
///
/// A snapshot is captured at most once per calendar day, so this is the series
/// a chart plots directly. Empty until the first capture — a new install has
/// no history, which is different from a flat one.
pub fn api_analytics_trend(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    // Captured on read rather than only from a scheduler: this crate has no
    // always-on poll loop yet, and a trend that only fills in while a
    // background task happens to be running would be empty on exactly the
    // installs most likely to look at it. Idempotent per day, so a page
    // refresh costs one indexed lookup.
    let _ = remind_me_core::analytics::capture_snapshot(conn);

    match remind_me_core::analytics::trend(conn) {
        Ok(snapshots) => ok(json!({ "snapshots": snapshots })),
        Err(e) => err(500, format!("analytics trend failed: {}", e)),
    }
}

/// Which builds are on each side of sync.
///
/// The node's own version is the point on a standalone install; `hub` exists
/// for the case the dashboard has no other way to see. Auth-gated by the
/// `/api/` prefix, unlike `/health`, and for a reason the reference states
/// plainly: this node's build is its own to publish, the hub's is another
/// machine's.
///
/// Best-effort throughout — an unreachable hub yields `null` rather than an
/// error, so the dashboard omits a line instead of rendering a failure into
/// its own chrome.
pub fn api_versions(
    _conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let node_id = remind_me_core::sync::configured_node_id();
    ok(json!({
        "version": remind_me_core::updater::INSTALLED_VERSION,
        // `""` means unconfigured, and an empty string in a UI reads as a
        // rendering bug rather than as an absence.
        "node_id": if node_id.is_empty() { Value::Null } else { Value::String(node_id) },
        "sync_enabled": remind_me_core::sync::sync_enabled(),
        "hub": remind_me_core::sync::probe_hub_version(),
    }))
}

// ---------------------------------------------------------------------------
// Dashboard (#78)
// ---------------------------------------------------------------------------

/// The dashboard's source, in dependency order.
///
/// This began as a single vendored `App.jsx`, which had grown to 1,664 lines
/// and 41 top-level definitions holding the fetch layer, the icon set, every
/// store, every form and the whole shell in one scope. Splitting it is a pure
/// move — the same definitions, reordered only to put the shared style tokens
/// beside the theme they are built from.
///
/// **The order of this array is load-bearing.** There is no bundler and no
/// module system here: each entry is emitted as its own
/// `<script type="text/babel">` block, and those share one global scope and
/// execute in document order. A file may therefore reference anything declared
/// by a file above it and nothing below it, which is why the shell comes last
/// and the theme comes first.
///
/// Kept as separate `<script>` blocks rather than concatenated into one: a
/// syntax error or a runtime throw then names the file it came from, instead
/// of an offset into a 1,600-line paste.
const DASHBOARD_SOURCES: &[(&str, &str)] = &[
    ("theme.jsx", include_str!("dashboard/theme.jsx")),
    ("api.jsx", include_str!("dashboard/api.jsx")),
    ("stores.jsx", include_str!("dashboard/stores.jsx")),
    ("icons.jsx", include_str!("dashboard/icons.jsx")),
    ("components.jsx", include_str!("dashboard/components.jsx")),
    ("forms.jsx", include_str!("dashboard/forms.jsx")),
    ("app.jsx", include_str!("dashboard/app.jsx")),
];

/// Every source above as its own Babel script block, each labelled with the
/// file it came from so a browser-console stack trace points at a real path.
fn dashboard_scripts() -> String {
    DASHBOARD_SOURCES
        .iter()
        .map(|(name, source)| {
            format!("<script type=\"text/babel\" data-file=\"{name}\">\n{source}\n</script>")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The reference's own `_build_dashboard_html()` wrapper: pinned CDN
/// React/ReactDOM/Babel builds (with the reference's own Subresource
/// Integrity hashes, HY-04 — a compromised or substituted CDN response cannot
/// execute), then the dashboard's own sources as `text/babel` blocks.
/// Requires network access to unpkg.com on first load, same limitation as the
/// reference: neither vendors the CDN assets themselves.
fn dashboard_html() -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Remind Me — Memory Dashboard</title>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ background: #0a0a0f; color: #e4e4ed; font-family: 'IBM Plex Sans', -apple-system, BlinkMacSystemFont, sans-serif; }}
  @import url('https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600;700&family=IBM+Plex+Sans:wght@400;500;600;700&display=swap');
  ::-webkit-scrollbar {{ width: 6px; }}
  ::-webkit-scrollbar-track {{ background: transparent; }}
  ::-webkit-scrollbar-thumb {{ background: #2a2a3a; border-radius: 3px; }}
  ::selection {{ background: rgba(99,102,241,0.25); }}
</style>
</head>
<body>
<div id="root"></div>
<script src="https://unpkg.com/react@18.3.1/umd/react.production.min.js"
        integrity="sha384-DGyLxAyjq0f9SPpVevD6IgztCFlnMF6oW/XQGmfe+IsZ8TqEiDrcHkMLKI6fiB/Z"
        crossorigin="anonymous"></script>
<script src="https://unpkg.com/react-dom@18.3.1/umd/react-dom.production.min.js"
        integrity="sha384-gTGxhz21lVGYNMcdJOyq01Edg0jhn/c22nsx0kyqP0TxaV5WVdsSH1fSDUf5YJj1"
        crossorigin="anonymous"></script>
<script src="https://unpkg.com/@babel/standalone@7.29.7/babel.min.js"
        integrity="sha384-ezQ6HS3FLspd9te19o2McUV6FAK091+GG7KO54f/R8DKgCDi7fULhapNrd5LY+vG"
        crossorigin="anonymous"></script>
{scripts}
</body>
</html>"#,
        scripts = dashboard_scripts()
    )
}

/// Serve the dashboard as a single-page app — the reference's own
/// `Route("/", index)`, part of the same routes/middleware set as `/api/*`
/// (this crate's `ROUTES` table and CORS policy, not a separate server).
pub fn dashboard(
    _conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    (
        200,
        Body::Raw {
            content_type: "text/html; charset=utf-8",
            payload: dashboard_html(),
        },
    )
}

// ---------------------------------------------------------------------------
// Stats / vitality
// ---------------------------------------------------------------------------

/// `stats::collect_dashboard` — the dashboard's own shape (`total`, `imports`,
/// `tags`), not `remind_me_stats`'s. Using the wrong one here previously left
/// the dashboard header, sidebar total, and "Unique Tags" card reading `0`
/// via their JS `||0` fallbacks (`stats.total`/`stats.tags` were absent, not
/// wrong), with nothing erroring to surface it.
pub fn api_stats(conn: &Connection, _wiki: &Wiki, _req: &Request, _params: &Params) -> (u16, Body) {
    match stats::collect_dashboard(conn) {
        Ok(s) => ok(s),
        Err(e) => internal_err(e),
    }
}

/// `vitality::build_vitality_report`, shared with `remind_me_vitality_report`.
pub fn api_vitality(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    match vitality::build_vitality_report(conn) {
        Ok(report) => ok(report),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Memory CRUD
// ---------------------------------------------------------------------------

/// `queries::list_memories`, reused verbatim — so this route's `limit` cap is
/// this crate's existing [`LIST_LIMIT_MAX`] (100), not the reference's 200:
/// one core function, one bound, across MCP and HTTP.
pub fn api_list(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let limit = match int_query(req, "limit", 20) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let offset = match int_query(req, "offset", 0) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let input = MemoryListInput {
        category: req.query_str("category").map(str::to_string),
        source: req.query_str("source").map(str::to_string),
        tags: req.query_list("tags"),
        // The dashboard is the same single user, but "don't surface by
        // default" means the same thing here as it does over MCP: opt in
        // explicitly with ?include_sensitive=1 or it stays hidden.
        include_sensitive: req
            .query_str("include_sensitive")
            .is_some_and(|v| matches!(v, "1" | "true" | "yes")),
        limit,
        offset,
        response_format: Default::default(),
    };
    match queries::list_memories(conn, &input) {
        Ok(result) => {
            let has_more = result.total > result.offset + result.memories.len();
            ok(json!({
                "total": result.total,
                "count": result.memories.len(),
                "offset": result.offset,
                "limit": result.limit,
                "has_more": has_more,
                "memories": result.memories,
            }))
        }
        Err(e) => internal_err(e),
    }
}

/// `queries::add_memory`, reused verbatim.
pub fn api_add(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let Some(content) = body.get("content").and_then(Value::as_str).map(str::trim) else {
        return err(400, "'content' is required");
    };
    if content.is_empty() {
        return err(400, "'content' is required");
    }

    let input = MemoryAddInput {
        sensitive: body
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        content: content.to_string(),
        category: body
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("general")
            .to_string(),
        tags: body
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        source: body
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("manual")
            .to_string(),
        metadata: body.get("metadata").cloned().unwrap_or(json!({})),
        subject: None,
        predicate: None,
        object: None,
        entities: Vec::new(),
    };
    match queries::add_memory(conn, input) {
        Ok(memory) => (
            201,
            Body::Json(serde_json::to_value(memory).unwrap_or(json!({}))),
        ),
        Err(e) => internal_err(e),
    }
}

/// Paginated FTS search behind `queries::search_paginated`, with an optional
/// `entity:` token extracted from `q` — `FT-04` parity with the structured
/// query syntax the MCP `remind_me_search` tool does not yet implement (a
/// separate, still-open gap; the extraction itself,
/// [`remind_me_core::fts::extract_entity_token`], is written to be shared
/// once that lands, rather than reimplemented a second time here).
pub fn api_search(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let raw_query = req.query_str("q").unwrap_or("").trim();
    if raw_query.is_empty() {
        return err(400, "Missing 'q' parameter");
    }
    let limit = match int_query(req, "limit", 50) {
        Ok(v) => v.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX),
        Err(e) => return e,
    };
    let offset = match int_query(req, "offset", 0) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let (entity, query) = core::fts::extract_entity_token(raw_query);
    let input = SearchPageInput {
        query,
        category: req.query_str("category").map(str::to_string),
        tags: req.query_list("tags"),
        entity,
        limit,
        offset,
    };
    match queries::search_paginated(conn, &input) {
        Ok(result) => ok(result),
        Err(e) => internal_err(e),
    }
}

pub fn api_get(conn: &Connection, _wiki: &Wiki, _req: &Request, params: &Params) -> (u16, Body) {
    let id = &params["memory_id"];
    match queries::get_memory_by_id(conn, id) {
        Ok(Some(memory)) => ok(memory),
        Ok(None) => err(404, "Not found"),
        Err(e) => internal_err(e),
    }
}

/// `queries::update_memory`, reused verbatim.
pub fn api_update(conn: &Connection, _wiki: &Wiki, req: &Request, params: &Params) -> (u16, Body) {
    let id = params["memory_id"].clone();
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let input = MemoryUpdateInput {
        memory_id: id,
        // Deliberately not read from the body: the reference's own
        // `api_update` (`api.py:1047`) handles content/category/source/tags/
        // metadata/sensitive and nothing else, so `clear_superseded` is an
        // MCP-tool affordance there, not an HTTP one. Exposing it here would
        // be a route this crate has and `remind_me` does not.
        clear_superseded: false,
        // Absent means "leave it alone", so a PATCH that does not mention the
        // flag cannot clear it.
        sensitive: body.get("sensitive").and_then(Value::as_bool),
        content: body
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string),
        category: body
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: body.get("tags").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        }),
        metadata: body.get("metadata").cloned(),
    };
    match queries::update_memory(conn, &input) {
        Ok(UpdateOutcome::Updated(memory)) => ok(*memory),
        Ok(UpdateOutcome::NotFound) => err(404, "Not found"),
        Ok(UpdateOutcome::NoFields) => err(400, "No fields to update"),
        Err(e) => internal_err(e),
    }
}

/// `queries::delete_memory`, reused verbatim. A hard delete — this crate has
/// no sync layer to tombstone for, matching how `delete_memory` already
/// behaves for the MCP tool.
pub fn api_delete(conn: &Connection, _wiki: &Wiki, _req: &Request, params: &Params) -> (u16, Body) {
    let id = &params["memory_id"];
    match queries::delete_memory(conn, id) {
        Ok(true) => ok(json!({ "deleted": id })),
        Ok(false) => err(404, "Not found"),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Bulk operations — HTTP-only; no MCP-tool equivalent
// ---------------------------------------------------------------------------

/// A caller-supplied id list from a JSON body's `ids` field, validated the
/// same way for every bulk route.
fn bulk_ids(body: &Value) -> Result<Vec<String>, (u16, Body)> {
    let Some(ids) = body.get("ids").and_then(Value::as_array) else {
        return Err(err(400, "'ids' must be a non-empty array of memory ids"));
    };
    if ids.is_empty() {
        return Err(err(400, "'ids' must be a non-empty array of memory ids"));
    }
    if ids.len() > BULK_IDS_MAX {
        return Err(err(
            400,
            format!("'ids' exceeds the {}-id limit per request", BULK_IDS_MAX),
        ));
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match id.as_str().filter(|s| !s.is_empty()) {
            Some(s) => out.push(s.to_string()),
            None => return Err(err(400, "'ids' must be an array of non-empty strings")),
        }
    }
    Ok(out)
}

pub fn api_bulk_delete(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let ids = match bulk_ids(&body) {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    match queries::bulk_delete(conn, &ids) {
        Ok(result) => ok(result),
        Err(e) => internal_err(e),
    }
}

pub fn api_bulk_tag(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let ids = match bulk_ids(&body) {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let Some(tags) = body.get("tags").and_then(Value::as_array) else {
        return err(400, "'tags' must be a non-empty array of non-empty strings");
    };
    if tags.is_empty() {
        return err(400, "'tags' must be a non-empty array of non-empty strings");
    }
    let mut tag_strings = Vec::with_capacity(tags.len());
    for tag in tags {
        match tag.as_str().filter(|s| !s.is_empty()) {
            Some(s) => tag_strings.push(s.to_string()),
            None => return err(400, "'tags' must be a non-empty array of non-empty strings"),
        }
    }
    let mode = match body.get("mode") {
        None | Some(Value::Null) => Default::default(),
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(mode) => mode,
            Err(_) => return err(400, "'mode' must be one of: add, remove, set"),
        },
    };

    match queries::bulk_tag(
        conn,
        &BulkTagInput {
            ids,
            tags: tag_strings,
            mode,
        },
    ) {
        Ok(result) => ok(result),
        Err(e) => internal_err(e),
    }
}

/// `queries::reclassify_memories`, reused verbatim. Capped at
/// [`BULK_IDS_MAX`] rather than the MCP tool's own `RECLASSIFY_BATCH_MAX`
/// (100) — the reference draws this as a distinct, dashboard-facing bound
/// (`_MAX_BULK_IDS`), and this mirrors that rather than reusing the
/// MCP-specific constant for an unrelated surface.
pub fn api_bulk_reclassify(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let input: ReclassifyInput = match parsed_body(req) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let count = input.classifications.len();
    if count == 0 {
        return err(400, "'classifications' must be a non-empty array");
    }
    if count > BULK_IDS_MAX {
        return err(
            400,
            format!(
                "'classifications' exceeds the {}-entry limit per request",
                BULK_IDS_MAX
            ),
        );
    }
    for c in &input.classifications {
        if c.memory_id.is_empty() || c.memory_type.is_empty() {
            return err(
                400,
                "each classification must be {'memory_id': str, 'memory_type': str}",
            );
        }
    }
    match queries::reclassify_memories(conn, &input) {
        Ok(result) => ok(result),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

pub fn api_entity(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let name = req.query_str("name").unwrap_or("").trim();
    if name.is_empty() {
        return err(400, "Missing 'name' parameter");
    }
    let limit = match int_query(req, "limit", 20) {
        Ok(v) => v.min(100),
        Err(e) => return e,
    };
    match entity_profile(conn, name, limit) {
        Ok(Some(profile)) => ok(profile),
        Ok(None) => err(404, format!("No entity found matching {:?}", name)),
        Err(e) => internal_err(e),
    }
}

pub fn api_entities(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let limit = match int_query(req, "limit", 50) {
        Ok(v) => v.min(200),
        Err(e) => return e,
    };
    let offset = match int_query(req, "offset", 0) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match list_entities(conn, limit, offset) {
        Ok(page) => ok(page),
        Err(e) => internal_err(e),
    }
}

/// `entity::traverse_from_name`, shared with `remind_me_entity_traverse` —
/// the reference explicitly notes its own traversal helper is shared the
/// same way, and this crate has had one function for both since #16.
pub fn api_entity_traverse(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let name = req.query_str("name").unwrap_or("").trim();
    if name.is_empty() {
        return err(400, "Missing 'name' parameter");
    }
    let hops = match int_query(req, "hops", 1) {
        Ok(v) => v.clamp(1, 3) as u32,
        Err(e) => return e,
    };
    let cap = match int_query(req, "cap", 20) {
        Ok(v) => v.min(100),
        Err(e) => return e,
    };
    let relation = req.query_str("relation").map(str::to_string);

    let input = EntityTraverseInput {
        name: name.to_string(),
        hops,
        relation,
        cap,
    };
    match traverse_from_name(conn, &input) {
        Ok(result) if !result.found => err(404, result.message.unwrap_or_default()),
        Ok(result) => ok(result),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Import / export
// ---------------------------------------------------------------------------

/// Import a file or directory, sharing containment (`SE-02`) and parsing with
/// the MCP import tools rather than duplicating either: this resolves and
/// probes the path only to decide which of [`importer::import_chat`] /
/// [`importer::import_directory`] to call, and both of those run the real,
/// authoritative containment check themselves before touching anything.
pub fn api_import(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let chat_input: ChatImportInput = match parsed_body(req) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let file_path = chat_input.file_path.trim();
    if file_path.is_empty() {
        return err(400, "'file_path' is required");
    }

    let resolved = import_paths::resolve_lexically(Path::new(file_path));
    if !import_paths::is_contained(&resolved, &import_paths::import_roots()) {
        return err(400, ImportPathError::OutsideRoots(resolved).to_string());
    }
    if !resolved.exists() {
        return err(400, ImportPathError::NotFound(resolved).to_string());
    }

    if resolved.is_dir() {
        let dir_input = BulkImportDirInput {
            directory: chat_input.file_path,
            category: chat_input.category,
            tags: chat_input.tags,
            extract_mode: chat_input.extract_mode,
            max_length: chat_input.max_length,
            recursive: true,
            kind: chat_input.kind,
        };
        match importer::import_directory(conn, &dir_input) {
            Ok(summary) => ok(summary),
            Err(e) => internal_err(e),
        }
    } else {
        match importer::import_chat(conn, &chat_input) {
            Ok(outcome) => ok(outcome),
            Err(e) => internal_err(e),
        }
    }
}

/// Export memories, optionally with the entity graph, inline or to a file.
///
/// Without `file_path`, this bypasses [`export::export_memories`] and calls
/// [`export::collect_export_records`] / [`export::render_export`] directly —
/// matching the reference exactly, which does the same for the same reason:
/// the inline case returns the export's own bytes as the response body, not
/// a JSON-wrapped summary.
pub fn api_export(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let format = match req.query_str("format").unwrap_or("json") {
        "json" => ExportFormat::Json,
        "jsonl" => ExportFormat::Jsonl,
        other => {
            return err(
                400,
                format!("Invalid format {:?}: use 'json' or 'jsonl'", other),
            )
        }
    };
    let category = req.query_str("category").map(str::to_string);
    let tags = req.query_list("tags");
    let file_path = req.query_str("file_path").map(str::to_string);
    let include_graph = req.query_bool_default_true("include_graph");
    // Unlike `clear_superseded` on the update route, the reference *does*
    // expose this one over HTTP (`api.py:1383`, "default false —
    // soft-deleted/superseded"), so it is a query parameter here too.
    let include_deleted = req.query_bool_default_false("include_deleted");

    let input = ExportInput {
        format,
        category,
        tags,
        file_path: file_path.clone(),
        include_graph,
        include_deleted,
    };

    if file_path.is_some() {
        match export::export_memories(conn, &input) {
            Ok(result) => ok(result),
            Err(e) => err(400, e.to_string()),
        }
    } else {
        match export::collect_export_records(conn, &input) {
            Ok(records) => {
                let payload = export::render_export(&records, format);
                let content_type = match format {
                    ExportFormat::Json => "application/json",
                    ExportFormat::Jsonl => "application/x-ndjson",
                };
                (
                    200,
                    Body::Raw {
                        content_type,
                        payload,
                    },
                )
            }
            Err(e) => internal_err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Wiki — read paths (FT-08); write paths added below
// ---------------------------------------------------------------------------
//
// The wiki tools are LLM-curated by design (see `wiki_fs::DEFAULT_SCHEMA`,
// seeded as the vault's `schema.md` on first read: "Distil, do not paste",
// "Revise in place"): Claude writes and browses it, and these read routes
// exist so a human owner can *see* it outside the MCP tools.
//
// The write paths that follow are a deliberate reversal of this section's
// original "there is deliberately no POST/PUT/DELETE here", not an oversight
// of it, and the reasoning is worth stating because the old note was right on
// its own terms. "LLM-curated" describes who does the *synthesis*, and that has
// not changed: `Wiki::compile` still returns a brief for a model to act on, and
// nothing here writes a page's prose on its own. What the read-only posture
// also did, though, was leave the owner of the vault unable to fix a typo, kill
// a page a compile pass got wrong, or write a page by hand without opening an
// MCP client — an editorial veto they plainly should have over their own notes.
// So the routes below hand over the mechanical operations (write, delete,
// advance the watermark) and leave the judgment where it was.
//
// Containment is structural rather than checked here: every path is
// `wiki_root/{slug}.md` where the slug comes from `wiki_import::slugify`, which
// keeps ASCII alphanumerics, turns everything else into `-`, and falls back to
// `page` when nothing survives. A title cannot therefore contain a separator, a
// `..`, a leading dot, or an extension, whatever a caller sends. Core refuses
// the reserved `index`/`log`/`schema` slugs on top of that, since those are
// generated and a hand-written `index.md` would put the index permanently at
// odds with the pages it claims to list.

pub fn api_wiki_pages(
    conn: &Connection,
    wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    match wiki.list_pages(conn) {
        Ok(pages) => ok(json!({ "count": pages.len(), "pages": pages })),
        Err(e) => internal_err(e),
    }
}

pub fn api_wiki_status(
    conn: &Connection,
    wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let pages = match wiki.list_pages(conn) {
        Ok(pages) => pages.len(),
        Err(e) => return internal_err(e),
    };
    match pending_compile_count(conn) {
        Ok(pending) => ok(json!({ "pages": pages, "pending_compile": pending })),
        Err(e) => internal_err(e),
    }
}

pub fn api_wiki_search(
    conn: &Connection,
    wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let query = req.query_str("q").unwrap_or("").trim();
    if query.is_empty() {
        return err(400, "Missing 'q' parameter");
    }
    let limit = match int_query(
        req,
        "limit",
        remind_me_core::wiki::WIKI_SEARCH_LIMIT_DEFAULT,
    ) {
        Ok(v) => v.clamp(
            remind_me_core::wiki::WIKI_SEARCH_LIMIT_MIN,
            remind_me_core::wiki::WIKI_SEARCH_LIMIT_MAX,
        ),
        Err(e) => return e,
    };
    match wiki.search_pages(conn, query, limit) {
        Ok(results) => ok(json!({ "count": results.len(), "results": results })),
        Err(e) => internal_err(e),
    }
}

pub fn api_wiki_load(
    conn: &Connection,
    wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let token_budget = match int_query(req, "token_budget", 0) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let include_index = req.query_bool_default_true("include_index");
    match wiki.load(conn, token_budget, include_index) {
        Ok(loaded) => ok(loaded),
        Err(e) => internal_err(e),
    }
}

pub fn api_wiki_page(
    conn: &Connection,
    wiki: &Wiki,
    _req: &Request,
    params: &Params,
) -> (u16, Body) {
    let slug = &params["slug"];
    match wiki.read_page(conn, slug) {
        Ok(Some(page)) => ok(page),
        Ok(None) => err(404, format!("Wiki page not found: {:?}", slug)),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Wiki — write paths
// ---------------------------------------------------------------------------

/// Longest accepted page title and body.
///
/// Core enforces neither: `Wiki::write_page` will write whatever it is handed.
/// These are `remind_me_wiki_write`'s own JSON-schema bounds (`maxLength` 200
/// and 100000), applied here so the two surfaces accept the same thing. A
/// bound that exists only on the tool would mean the same page could be
/// written over HTTP and then rejected as too long over MCP.
const WIKI_TITLE_MAX: usize = 200;
const WIKI_CONTENT_MAX: usize = 100_000;
/// `remind_me_wiki_write`'s `log_note` bound, applied for the same reason.
const WIKI_LOG_NOTE_MAX: usize = 500;

/// `Wiki::write_page`, shared with `remind_me_wiki_write` — create a page or
/// replace an existing one's body wholesale.
///
/// Title-addressed rather than slug-addressed, mirroring the tool exactly: the
/// slug *is* `slugify(title)`, so a caller-supplied slug disagreeing with the
/// title would name a file the index could not find. That also means retitling
/// a page writes a new one and leaves the old behind, which is core's existing
/// behaviour for the tool and not something this route invents a rename for.
///
/// 200 rather than 201 even when a page is created: the same request updates in
/// place when the slug already exists, so the status cannot promise creation.
/// The `created` flag in the body says which happened.
pub fn api_wiki_write(
    conn: &Connection,
    wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let body = match json_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let content = body.get("content").and_then(Value::as_str).unwrap_or("");
    if title.is_empty() {
        return err(400, "'title' is required");
    }
    if content.trim().is_empty() {
        return err(400, "'content' is required");
    }
    if title.chars().count() > WIKI_TITLE_MAX {
        return err(
            400,
            format!("'title' exceeds the {}-character limit", WIKI_TITLE_MAX),
        );
    }
    if content.chars().count() > WIKI_CONTENT_MAX {
        return err(
            400,
            format!("'content' exceeds the {}-character limit", WIKI_CONTENT_MAX),
        );
    }
    let log_note = body
        .get("log_note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if log_note.is_some_and(|n| n.chars().count() > WIKI_LOG_NOTE_MAX) {
        return err(
            400,
            format!(
                "'log_note' exceeds the {}-character limit",
                WIKI_LOG_NOTE_MAX
            ),
        );
    }

    match wiki.write_page(conn, title, content, log_note) {
        Ok(Ok(outcome)) => ok(outcome),
        // 403, not 400: the request is well-formed and the page is real, it is
        // simply not the caller's to write. `index`/`log`/`schema` are
        // regenerated from the pages themselves.
        Ok(Err(_)) => err(
            403,
            format!(
                "{:?} is a reserved system page and cannot be written directly",
                title
            ),
        ),
        Err(e) => internal_err(e),
    }
}

/// `Wiki::delete_page`, shared with `remind_me_wiki_delete` — removes the file,
/// its index rows and its outbound links, then regenerates `index.md`.
///
/// A hard delete of a file on disk with no undo, which is why the dashboard
/// puts a confirmation in front of it. Addressed by title *or* slug, since
/// `slugify` is idempotent over a slug.
pub fn api_wiki_delete(
    conn: &Connection,
    wiki: &Wiki,
    _req: &Request,
    params: &Params,
) -> (u16, Body) {
    let slug = &params["slug"];
    match wiki.delete_page(conn, slug) {
        Ok(core::WikiDeleteOutcome::Deleted) => ok(json!({ "deleted": slug })),
        Ok(core::WikiDeleteOutcome::NotFound) => {
            err(404, format!("Wiki page not found: {:?}", slug))
        }
        Ok(core::WikiDeleteOutcome::Reserved) => err(
            403,
            format!("{:?} is a reserved system page and cannot be deleted", slug),
        ),
        Err(e) => internal_err(e),
    }
}

/// `Wiki::compile`, shared with `remind_me_wiki_compile` — the two-phase
/// synthesis driver.
///
/// Phase one (`mark_integrated` absent or false) returns a **brief**: the raw
/// memories created since the watermark, the current page index and the
/// maintainer schema, assembled into a prompt. It advances nothing, so it is
/// safe to call repeatedly. Phase two (`mark_integrated: true`) moves the
/// watermark past the batch that was surfaced.
///
/// Worth being plain about what this route can and cannot do: the brief is
/// written for a model to act on, and this server has none. The dashboard
/// therefore shows the pending sources and offers the brief for copying into a
/// Claude session, rather than pretending a browser can synthesise the pages
/// itself. The mechanical half — seeing what is pending, and marking a batch
/// integrated once it has been written up — is genuinely usable from a browser,
/// and that is the half this exposes.
///
/// A `POST` even for phase one, which only reads: phase two mutates, both
/// phases take a JSON body, and splitting one tool across two methods by an
/// argument's value would be a shape a caller has to learn rather than read.
/// The cost is that the brief needs the API key on a server that has one set,
/// which is the correct side to err on for something that quotes raw memory
/// content back.
pub fn api_wiki_compile(
    conn: &Connection,
    wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    // An absent body means "brief me with the defaults", which is the common
    // call -- so an empty body is not a 400 the way a malformed one is.
    let body = if req.body.is_empty() {
        json!({})
    } else {
        match json_body(req) {
            Ok(b) => b,
            Err(e) => return e,
        }
    };
    let limit = match body.get("limit") {
        None | Some(Value::Null) => 20,
        Some(value) => match value.as_u64() {
            Some(n) => (n as usize).clamp(1, 100),
            None => return err(400, "'limit' must be a number between 1 and 100"),
        },
    };
    let mark_integrated = match body.get("mark_integrated") {
        None | Some(Value::Null) => false,
        Some(value) => match value.as_bool() {
            Some(b) => b,
            None => return err(400, "'mark_integrated' must be a boolean"),
        },
    };

    match wiki.compile(conn, limit, mark_integrated) {
        Ok(outcome) => ok(outcome),
        Err(e) => internal_err(e),
    }
}

/// `Wiki::read_schema` — the maintainer schema the compile brief embeds.
///
/// Read-only, and its own route rather than a field on `/api/wiki/status`: it
/// is the document that says how pages are supposed to be written, so an
/// editor wants it next to the editor, not buried in a status payload.
pub fn api_wiki_schema(
    _conn: &Connection,
    wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    match wiki.read_schema() {
        Ok(schema) => ok(json!({ "schema": schema })),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Reminders, digest, status, saved searches — the MCP tools a human can now
// also run from the dashboard
// ---------------------------------------------------------------------------
//
// Every handler below defers to the same `remind_me_core` function its MCP
// tool calls, so a reminder set from the dashboard and one set by Claude go
// through identical validation and write the same row. Nothing here reproduces
// the tools' logic — the only things that differ are how input arrives (query
// string / JSON body instead of a tool-call argument object) and how failure
// is reported (an HTTP status instead of `isError`).
//
// Read routes here sit behind the `/api/` prefix's usual posture: open when
// `REMIND_ME_API_KEY` is unset, gated once it is set. `/api/status` is the one
// worth naming explicitly — it reports the database and backup paths, the
// schema version and subsystem state, which is operational shape rather than
// memory content, and the same class of thing `/api/stats` already publishes
// via `db_path`.

/// `reminders::list_reminders`, shared with `remind_me_list_reminders` and
/// with the ICS feed — one query, so the dashboard, the calendar feed and the
/// tool cannot disagree about what is overdue.
///
/// `when` is `upcoming` (default), `overdue`, or `all`. An unrecognised value
/// is refused rather than silently defaulted: a typo'd window that quietly
/// answered "upcoming" would read as "no overdue reminders".
pub fn api_reminders(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let when = match req.query_str("when").unwrap_or("upcoming") {
        "upcoming" => ReminderWindow::Upcoming,
        "overdue" => ReminderWindow::Overdue,
        "all" => ReminderWindow::All,
        other => {
            return err(
                400,
                format!(
                    "Invalid window {:?}: use 'upcoming', 'overdue' or 'all'",
                    other
                ),
            )
        }
    };
    let limit = match int_query(req, "limit", 20) {
        Ok(v) => (v as i64).clamp(REMINDER_LIMIT_MIN, REMINDER_LIMIT_MAX),
        Err(e) => return e,
    };
    match remind_me_core::reminders::list_reminders(conn, when, limit) {
        Ok(memories) => ok(json!({
            "count": memories.len(),
            "memories": memories,
        })),
        Err(e) => internal_err(e),
    }
}

/// `reminders::set_reminder`, reused verbatim — set a reminder on a memory, or
/// clear one by omitting `remind_at` (or sending it as `null`, or as an empty
/// string, which is what a cleared form field sends).
///
/// The outcomes core reports are mapped onto the statuses that mean the same
/// thing over HTTP, rather than answering 200 with an `outcome` field a caller
/// has to read to notice nothing happened: a missing memory is 404, and a
/// timestamp that is unparseable or already past is 400. The body still
/// carries the full `SetReminderOutcome`, so the reason survives.
pub fn api_set_reminder(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let input: SetReminderInput = match parsed_body(req) {
        Ok(i) => i,
        Err(_) => return err(400, "'memory_id' is required"),
    };
    if input.memory_id.trim().is_empty() {
        return err(400, "'memory_id' is required");
    }
    match remind_me_core::reminders::set_reminder(
        conn,
        &input.memory_id,
        input.remind_at.as_deref(),
    ) {
        Ok(outcome) => {
            let status = match outcome {
                SetReminderOutcome::NotFound { .. } => 404,
                SetReminderOutcome::Rejected { .. } => 400,
                SetReminderOutcome::Set { .. } | SetReminderOutcome::Cleared { .. } => 200,
            };
            (
                status,
                Body::Json(serde_json::to_value(outcome).unwrap_or(json!({}))),
            )
        }
        Err(e) => internal_err(e),
    }
}

/// `digest::build_digest`, shared with `remind_me_digest` — recent memories,
/// vitality, reminders on both sides of now, and sync state in one payload.
///
/// Sensitive memories are excluded by the core function with no override, and
/// this route deliberately adds none: a digest is the ambient surface that
/// flag exists to keep things off, and a dashboard panel is more ambient than
/// a tool call, not less.
pub fn api_digest(conn: &Connection, _wiki: &Wiki, req: &Request, _params: &Params) -> (u16, Body) {
    let since_days = match int_query(req, "since_days", digest::DEFAULT_SINCE_DAYS as usize) {
        Ok(v) => (v as i64).clamp(digest::DIGEST_SINCE_DAYS_MIN, digest::DIGEST_SINCE_DAYS_MAX),
        Err(e) => return e,
    };
    match digest::build_digest(conn, since_days) {
        Ok(data) => ok(data),
        Err(e) => internal_err(e),
    }
}

/// `status::server_status`, shared with `remind_me_server_status` — the build
/// actually serving the request, where the database lives, schema currency,
/// backup state, and which subsystems are running.
///
/// One field is overridden, the same way the MCP tool overrides several of
/// its own: core reports `dashboard` as not-implemented because *it* has no
/// way to see across processes to the `rusty-remind-me api` daemon. From
/// inside that daemon the question does not arise — the process answering
/// this request is the dashboard — so answering "not implemented" here would
/// be a report contradicted by the fact that it was delivered.
///
/// Deliberately *not* by reusing `pid::dashboard_status`, which the MCP tool
/// uses for the genuinely cross-process case: that probes the recorded URL
/// over HTTP, and this server answers one connection at a time. A request to
/// itself could not be accepted until the request making it had finished, so
/// the probe would block until it timed out. The MCP tool is a different
/// process and has no such problem.
pub fn api_status(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    match remind_me_core::status::server_status(conn) {
        Ok(status) => {
            let mut report = serde_json::to_value(&status).unwrap_or(json!({}));
            report["dashboard"] =
                serde_json::to_value(SubsystemStatus::Active).unwrap_or(json!({}));
            ok(report)
        }
        Err(e) => internal_err(e),
    }
}

/// `saved_searches::list_saved_searches`, reused verbatim.
pub fn api_saved_searches(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    _params: &Params,
) -> (u16, Body) {
    match saved_searches::list_saved_searches(conn) {
        Ok(searches) => ok(json!({ "count": searches.len(), "saved_searches": searches })),
        Err(e) => internal_err(e),
    }
}

/// `saved_searches::save_search`, reused verbatim — create, or update in place
/// when the name already exists, which is how core defines re-saving a name.
///
/// Answers 200 rather than 201 for that reason: this route cannot tell a
/// caller it created something when the same request may well have updated a
/// row that was already there.
pub fn api_save_search(
    conn: &Connection,
    _wiki: &Wiki,
    req: &Request,
    _params: &Params,
) -> (u16, Body) {
    let input: SaveSearchInput = match parsed_body(req) {
        Ok(i) => i,
        Err(_) => return err(400, "'name' and 'query' are required"),
    };
    if input.name.trim().is_empty() || input.query.trim().is_empty() {
        return err(400, "'name' and 'query' are required");
    }
    match saved_searches::save_search(conn, &input) {
        Ok(saved) => ok(saved),
        Err(e) => internal_err(e),
    }
}

/// `saved_searches::run_saved_search`, reused verbatim.
///
/// A `GET`: running a saved search only reads, so it stays available on an
/// unauthenticated server exactly as `/api/memories/search` does — the stored
/// query is a shorthand for that same search, and gating one but not the other
/// would be a distinction without a difference.
pub fn api_run_saved_search(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    params: &Params,
) -> (u16, Body) {
    let name = &params["name"];
    match saved_searches::get_saved_search(conn, name) {
        Ok(Some(saved)) => match saved_searches::run_saved_search(conn, &saved) {
            Ok(results) => ok(json!({
                "name": saved.name,
                "query": saved.query,
                "count": results.len(),
                "results": results,
            })),
            Err(e) => internal_err(e),
        },
        Ok(None) => err(404, format!("Saved search {:?} not found", name)),
        Err(e) => internal_err(e),
    }
}

/// `saved_searches::delete_saved_search`, reused verbatim — the stored search
/// and the seen-memory rows its watch tracking accumulated.
pub fn api_delete_saved_search(
    conn: &Connection,
    _wiki: &Wiki,
    _req: &Request,
    params: &Params,
) -> (u16, Body) {
    let name = &params["name"];
    match saved_searches::delete_saved_search(conn, name) {
        Ok(true) => ok(json!({ "deleted": name })),
        Ok(false) => err(404, format!("Saved search {:?} not found", name)),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// The route table
// ---------------------------------------------------------------------------
//
// Order matters: routes are matched top-to-bottom, first-match-wins. Literal
// paths that could otherwise be swallowed by a `{param}` sibling — `/search`,
// `/bulk/*`, `/status`, `/load` — are listed before the templated route they
// would collide with, mirroring the reference's own Starlette registration
// order and for the same reason.
pub const ROUTES: &[Route] = &[
    Route {
        methods: &["GET"],
        pattern: "/health",
        handler: health,
    },
    Route {
        methods: &["GET"],
        pattern: "/",
        handler: dashboard,
    },
    Route {
        methods: &["GET"],
        pattern: "/metrics",
        handler: metrics,
    },
    Route {
        methods: &["GET"],
        pattern: "/manifest.json",
        handler: manifest,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/analytics/trend",
        handler: api_analytics_trend,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/versions",
        handler: api_versions,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/stats",
        handler: api_stats,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/vitality",
        handler: api_vitality,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/digest",
        handler: api_digest,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/status",
        handler: api_status,
    },
    // `/api/reminders` is three segments; the ICS feed's
    // `/api/reminders/{token}.ics` is four and is answered before dispatch
    // ever runs, so the two cannot shadow each other.
    Route {
        methods: &["GET"],
        pattern: "/api/reminders",
        handler: api_reminders,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/reminders",
        handler: api_set_reminder,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/saved-searches",
        handler: api_saved_searches,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/saved-searches",
        handler: api_save_search,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/saved-searches/{name}/run",
        handler: api_run_saved_search,
    },
    Route {
        methods: &["DELETE"],
        pattern: "/api/saved-searches/{name}",
        handler: api_delete_saved_search,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/memories/search",
        handler: api_search,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/memories/bulk/delete",
        handler: api_bulk_delete,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/memories/bulk/tag",
        handler: api_bulk_tag,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/memories/bulk/reclassify",
        handler: api_bulk_reclassify,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/memories",
        handler: api_list,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/memories",
        handler: api_add,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/memories/{memory_id}",
        handler: api_get,
    },
    Route {
        methods: &["PUT", "PATCH"],
        pattern: "/api/memories/{memory_id}",
        handler: api_update,
    },
    Route {
        methods: &["DELETE"],
        pattern: "/api/memories/{memory_id}",
        handler: api_delete,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/import",
        handler: api_import,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/export",
        handler: api_export,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/entity",
        handler: api_entity,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/entities",
        handler: api_entities,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/entity/traverse",
        handler: api_entity_traverse,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki",
        handler: api_wiki_pages,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki/search",
        handler: api_wiki_search,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki/load",
        handler: api_wiki_load,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki/status",
        handler: api_wiki_status,
    },
    Route {
        methods: &["POST"],
        pattern: "/api/wiki",
        handler: api_wiki_write,
    },
    // Before `/api/wiki/{slug}`, per this table's stated first-match-wins
    // convention. Both are four segments, so a page whose title slugs to
    // `compile` or `schema` shares a path with these -- the same collision
    // `search`, `load` and `status` have had since they were added, and
    // resolved the same way: the literal wins for the method it declares, and
    // `GET /api/wiki/compile` still falls through to the page lookup.
    Route {
        methods: &["POST"],
        pattern: "/api/wiki/compile",
        handler: api_wiki_compile,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki/schema",
        handler: api_wiki_schema,
    },
    Route {
        methods: &["GET"],
        pattern: "/api/wiki/{slug}",
        handler: api_wiki_page,
    },
    Route {
        methods: &["DELETE"],
        pattern: "/api/wiki/{slug}",
        handler: api_wiki_delete,
    },
];

// ---------------------------------------------------------------------------
// The reminders calendar feed (issue #118)
// ---------------------------------------------------------------------------

/// `GET /api/reminders/{token}.ics` — the subscribable iCalendar feed.
///
/// Deliberately outside [`ROUTES`] and outside the `Authorization` gate every
/// other `/api/*` route sits behind. A calendar app's "subscribe by URL"
/// feature polls this from the provider's own servers, on a schedule the user
/// does not control, with no way to attach a custom header — so the credential
/// has to live in the URL. That is the *only* reason for the exemption, and
/// the token in the path is the whole credential.
///
/// A wrong token gets a bare 404, not a 401: a 401 would confirm that the
/// route exists and that a token was checked, which tells a prober they have
/// found the right shape and only need the secret. Matching the reference.
///
/// Compared with [`constant_time_eq`] rather than `==`. The token is long and
/// guessed a byte at a time by a timing oracle otherwise — the same reason
/// this crate already compares the API key that way.
pub fn api_reminders_ics(conn: &Connection, token: &str) -> (u16, Body) {
    let expected = remind_me_core::ics::resolve_ics_token();
    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        // Never log the supplied token: a rejected value is still a secret
        // somebody typed, and logs are the one place secrets outlive rotation.
        return (404, Body::Json(json!({ "error": "Not Found" })));
    }

    // Exactly the `all` window `remind_me_list_reminders` returns — upcoming
    // plus overdue-and-undelivered — by calling the same function rather than
    // repeating its SQL, so the feed and the tool cannot disagree about what
    // is on the calendar. Uncapped: a subscriber wants every reminder, not the
    // first page of them.
    match remind_me_core::reminders::list_reminders(
        conn,
        remind_me_core::models::ReminderWindow::All,
        i64::MAX,
    ) {
        Ok(reminders) => (
            200,
            Body::Raw {
                content_type: "text/calendar",
                payload: remind_me_core::ics::build_ics_now(&reminders),
            },
        ),
        Err(e) => internal_err(e),
    }
}

// ---------------------------------------------------------------------------
// Metrics and the PWA manifest (issue #119)
// ---------------------------------------------------------------------------

/// `GET /metrics` — Prometheus text exposition.
///
/// **404 while `REMIND_ME_METRICS_ENABLED` is unset**, rather than a 403 or an
/// empty 200. "Off" should be indistinguishable from "this build does not have
/// it", so a scrape configured against a server with metrics disabled fails
/// loudly instead of silently recording nothing.
///
/// **Unauthenticated, gated on the enable flag instead of a bearer token** —
/// the same posture as `/health`, and deliberate rather than an oversight.
/// Prometheus scrape configs typically send no custom headers, so requiring
/// one would mean hand-rolling a static-bearer scrape config for this single
/// target; and exposure is already opt-in at the config level, unlike
/// `/api/*`'s always-on surface.
///
/// The tradeoff is real and worth stating plainly: while enabled, this reveals
/// usage patterns — which tools are called and how often, search volume,
/// memory and outbox counts — to anyone who can reach the port. It exposes no
/// memory *content*. An operator who considers the shape sensitive should
/// place the port accordingly, which is how self-hosted exporters are
/// generally run.
pub fn metrics(
    conn: &Connection,
    _: &Wiki,
    _: &Request,
    _: &HashMap<String, String>,
) -> (u16, Body) {
    if !remind_me_core::metrics::metrics_enabled() {
        return (404, Body::Json(json!({ "error": "Not Found" })));
    }

    // Computed per scrape rather than shadowed as counters, so they cannot
    // drift from the tables they describe.
    let mut gauges = Vec::new();
    if let Ok(total) = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    ) {
        gauges.push(remind_me_core::metrics::GaugeSpec::new(
            "remind_me_memories_total",
            "Total non-deleted memories currently in the store.",
            total as f64,
        ));
    }
    if remind_me_core::sync::sync_enabled() {
        if let Ok(pending) = conn.query_row(
            "SELECT COUNT(*) FROM sync_outbox WHERE sent_at = ''",
            [],
            |r| r.get::<_, i64>(0),
        ) {
            gauges.push(remind_me_core::metrics::GaugeSpec::new(
                "remind_me_sync_outbox_pending",
                "Sync outbox rows not yet acknowledged by the hub.",
                pending as f64,
            ));
        }
    }

    (
        200,
        Body::Raw {
            content_type: "text/plain; version=0.0.4",
            payload: remind_me_core::metrics::render_prometheus_text(&gauges),
        },
    )
}

/// `GET /manifest.json` — the dashboard's Web App Manifest.
///
/// Unauthenticated like `/` and `/health`: a browser fetches a `<link
/// rel="manifest">` with no `Authorization` header, so requiring one would
/// simply mean the manifest never loads. It carries no user data.
pub fn manifest(_: &Connection, _: &Wiki, _: &Request, _: &HashMap<String, String>) -> (u16, Body) {
    (
        200,
        Body::Raw {
            content_type: "application/manifest+json",
            payload: remind_me_core::metrics::manifest_json().to_string(),
        },
    )
}
