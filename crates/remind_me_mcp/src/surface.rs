//! The public tool surface (ADR-0030): what `tools/list` advertises, and
//! which handler a public call reaches.
//!
//! Handlers keep their pre-0.5 names in [`crate::handler_catalog`] and the
//! dispatch `match`. A kept tool is its own handler. A merged tool picks one
//! of several handlers from its arguments in [`route`], and its schema is the
//! union of theirs, built in [`public_tools`]. An alias (an old name) is not
//! routed at all: it reaches its own handler with its old arguments, so a
//! caller using it sees no change before the alias is removed in 0.6.0.

use crate::references::KINDS;
use remind_me_core::tool_profiles::GROUPS;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// A public call after routing: the handler that runs it and the arguments
/// that handler reads.
#[derive(Debug, PartialEq)]
pub struct Route {
    pub handler: String,
    pub args: Value,
}

/// Route a `tools/call`. Names that are not merged pass through unchanged.
///
/// An error is a caller mistake (an unknown `kind`, two exclusive flags) and
/// is returned as the tool's error text.
pub fn route(tool: &str, mut args: Value) -> Result<Route, String> {
    let handler = match tool {
        "remind_me_search" => search(&mut args),
        "remind_me_get" => get(&mut args),
        "remind_me_update" => update(&args),
        "remind_me_digest" => pick(&args, "section", Some("recent"), DIGEST),
        "remind_me_server_status" => pick(&args, "section", Some("overview"), STATUS),
        "remind_me_queue" => queue(&mut args),
        "remind_me_wiki_read" => Ok(wiki_read(&args)),
        "remind_me_import" => import(&mut args),
        "remind_me_reminders" => Ok(if flag(&args, "ics_url") {
            "remind_me_reminders_ics_url"
        } else {
            "remind_me_list_reminders"
        }),
        "remind_me_saved_search" => pick(&args, "action", None, SAVED_SEARCH),
        "remind_me_sync_status" => pick(&args, "reconcile", Some("none"), SYNC),
        other => {
            note_alias(other);
            return Ok(Route {
                handler: other.to_string(),
                args,
            });
        }
    }?;
    Ok(Route {
        handler: handler.to_string(),
        args,
    })
}

type Choices = &'static [(&'static str, &'static str)];

const DIGEST: Choices = &[
    ("recent", "remind_me_digest"),
    ("stats", "remind_me_stats"),
    ("vitality", "remind_me_vitality_report"),
];

const STATUS: Choices = &[
    ("overview", "remind_me_server_status"),
    ("watcher", "remind_me_watch_status"),
    ("webhook", "remind_me_webhook_status"),
    ("updates", "remind_me_check_update"),
    ("connectors", "remind_me_list_connectors"),
];

/// The `*_batch` fetchers take `batch_size`; the rest take `limit`.
const QUEUE: Choices = &[
    ("decompose", "remind_me_decompose_batch"),
    ("extract", "remind_me_extract_batch"),
    ("normalize", "remind_me_normalize_batch"),
    ("reclassify", "remind_me_reclassify_batch"),
    ("recalibrate", "remind_me_recalibrate_candidates"),
    ("contradictions", "remind_me_contradiction_candidates"),
    ("promotion", "remind_me_promotion_candidates"),
    ("stale", "remind_me_stale_candidates"),
];

const IMPORT_FROM: Choices = &[
    ("file", "remind_me_import_chat"),
    ("directory", "remind_me_import_directory"),
    ("dbs", "remind_me_import_dbs"),
    ("mempalace", "remind_me_import_mempalace"),
    ("wiki", "remind_me_wiki_import"),
];

/// What `path` is called by each import handler.
const IMPORT_PATH: Choices = &[
    ("remind_me_import_chat", "file_path"),
    ("remind_me_import_directory", "directory"),
    ("remind_me_import_dbs", "db_path"),
    ("remind_me_wiki_import", "dir"),
];

const SAVED_SEARCH: Choices = &[
    ("save", "remind_me_save_search"),
    ("list", "remind_me_list_saved_searches"),
    ("run", "remind_me_run_saved_search"),
    ("delete", "remind_me_delete_saved_search"),
];

const SYNC: Choices = &[
    ("none", "remind_me_sync_status"),
    ("hub", "remind_me_sync_reconcile"),
    ("peer", "remind_me_sync_reconcile_peer"),
];

/// A non-empty string argument.
fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Move `from` to `to`, unless `to` is already set.
fn rename(args: &mut Value, from: &str, to: &str) {
    let Some(object) = args.as_object_mut() else {
        return;
    };
    if let Some(value) = object.remove(from) {
        object.entry(to).or_insert(value);
    }
}

/// The handler `choices` maps `args[key]` to, or `default` when it is unset.
fn pick(
    args: &Value,
    key: &str,
    default: Option<&str>,
    choices: Choices,
) -> Result<&'static str, String> {
    let options = || {
        choices
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(wanted) = text(args, key).or(default) else {
        return Err(format!("`{key}` is required: one of {}", options()));
    };
    choices
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|(_, handler)| *handler)
        .ok_or_else(|| format!("unknown {key} {wanted:?}: one of {}", options()))
}

/// The only arguments a reference lookup honours. Any other search argument
/// is refused rather than silently dropped, so a scoped or shaped search
/// never quietly turns into something else, including filters search gains
/// later.
const FOR_REFERENCES: [&str; 3] = ["reference", "reference_kind", "include_sensitive"];

/// The only arguments a search without `query` honours: the listing's own
/// filters. Ranking and shaping options (`detail`, `token_budget`,
/// `min_confidence`, ...) are refused, for the same reason.
const FOR_LISTING: [&str; 12] = [
    "query",
    "category",
    "tags",
    "source",
    "limit",
    "offset",
    "response_format",
    "include_sensitive",
    "project",
    "branch",
    "session_id",
    "written_by",
];

/// The non-null arguments outside `allowed`, in argument order.
fn outside<'a>(args: &'a Value, allowed: &[&str]) -> Vec<&'a str> {
    args.as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| !value.is_null() && !allowed.contains(&key.as_str()))
        .map(|(key, _)| key.as_str())
        .collect()
}

fn search(args: &mut Value) -> Result<&'static str, String> {
    if text(args, "reference").is_some() || text(args, "reference_kind").is_some() {
        let others = outside(args, &FOR_REFERENCES);
        if !others.is_empty() {
            return Err(format!(
                "`reference` looks up references only, with `reference_kind` and \
                 `include_sensitive`; drop {}",
                others.join(", ")
            ));
        }
        rename(args, "reference", "value");
        rename(args, "reference_kind", "kind");
        return Ok("remind_me_references");
    }
    if text(args, "query").is_some() {
        return Ok("remind_me_search");
    }
    let others = outside(args, &FOR_LISTING);
    if !others.is_empty() {
        return Err(format!(
            "without `query` this lists memories newest first, which does not take {}; \
             pass a query or drop them",
            others.join(", ")
        ));
    }
    if let Some(object) = args.as_object_mut() {
        object.remove("query");
    }
    Ok("remind_me_list")
}

fn get(args: &mut Value) -> Result<&'static str, String> {
    if args.get("memory_ids").is_some() {
        if text(args, "capture_id").is_some() || flag(args, "history") || flag(args, "raw") {
            return Err(
                "`memory_ids` fetches memories only; drop capture_id, history and raw".to_string(),
            );
        }
        return Ok("remind_me_get");
    }
    if text(args, "capture_id").is_some() {
        return Ok("remind_me_get_capture");
    }
    let handler = match (flag(args, "history"), flag(args, "raw")) {
        (true, true) => return Err("set `history` or `raw`, not both".to_string()),
        (true, false) => "remind_me_history",
        (false, true) => "remind_me_source",
        (false, false) => return Ok("remind_me_get"),
    };
    rename(args, "id", "memory_id");
    Ok(handler)
}

/// A reminder is set by its own handler, so it is refused alongside other
/// edits rather than half-applied.
fn update(args: &Value) -> Result<&'static str, String> {
    let Some(object) = args.as_object().filter(|o| o.contains_key("remind_at")) else {
        return Ok("remind_me_update");
    };
    let others: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|k| !matches!(*k, "memory_id" | "remind_at" | "response_format"))
        .collect();
    if others.is_empty() {
        Ok("remind_me_set_reminder")
    } else {
        Err(format!(
            "send `remind_at` in its own call; this one also sets {}",
            others.join(", ")
        ))
    }
}

fn queue(args: &mut Value) -> Result<&'static str, String> {
    let handler = pick(args, "kind", None, QUEUE)?;
    if handler.ends_with("_batch") {
        rename(args, "limit", "batch_size");
    }
    Ok(handler)
}

fn wiki_read(args: &Value) -> &'static str {
    if text(args, "slug").is_some() {
        "remind_me_wiki_read"
    } else if flag(args, "all") {
        "remind_me_wiki_load"
    } else if text(args, "query").is_some() {
        "remind_me_wiki_search"
    } else {
        "remind_me_wiki_list"
    }
}

fn import(args: &mut Value) -> Result<&'static str, String> {
    let handler = pick(args, "from", Some("file"), IMPORT_FROM)?;
    if let Some((_, name)) = IMPORT_PATH.iter().find(|(h, _)| *h == handler) {
        rename(args, "path", name);
    }
    Ok(handler)
}

/// Log an alias's first use in this process, naming its replacement, so the
/// move to 0.6.0 can be checked against real callers. Every call is also
/// counted under the name it was made with, by `record_tool_call`.
fn note_alias(name: &str) {
    let Some(new) = remind_me_core::tool_profiles::replacement(name) else {
        return;
    };
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if seen.insert(name.to_string()) {
        eprintln!("{name} is deprecated and is removed in 0.6.0; use {new}");
    }
}

/// The tools `tools/list` advertises, in group order, built from `catalog`.
///
/// A name with no definition is skipped rather than listed empty; a test
/// asserts there is none.
pub fn public_tools(catalog: &[Value]) -> Vec<Value> {
    GROUPS
        .iter()
        .flat_map(|(_, tools)| tools.iter())
        .filter_map(|name| definition(name, catalog))
        .collect()
}

fn definition(name: &str, catalog: &[Value]) -> Option<Value> {
    let merged = |description: &str, handlers: &[&str], drop: &[&str], extra: Value| {
        Some(merge(name, description, catalog, handlers, drop, extra))
    };
    match name {
        "remind_me_search" => merged(
            "Search remind-me memories by keyword and meaning; call this first to recall anything \
             about the user, their projects or past decisions. Hits are compact by default (id, \
             snippet, score); fetch whole memories with remind_me_get `memory_ids`, or pass \
             `detail=full`. Without `query` it lists memories newest first, filtered by \
             category, tags, source, project, branch, session or writer. With `reference` it finds the memories that point at an issue, commit, URL \
             or path.",
            &["remind_me_search", "remind_me_list"],
            &[],
            json!({
                "query": { "type": "string", "description": "What to look for. Omit to list newest first." },
                "reference": { "type": "string", "description": "Find memories pointing at this: owner/repo#123, a commit sha, a path, a URL, @handle." },
                "reference_kind": { "type": "string", "enum": KINDS, "description": "Narrow `reference` to one kind, or give it alone for every reference of that kind." }
            }),
        ),
        "remind_me_get" => merged(
            "Get a remind-me memory by `memory_id`, several at once by `memory_ids` (such as the \
             ids a compact search returned), or a captured conversation's dialog and summary by \
             `capture_id`. `history=true` lists a memory's earlier versions; `raw=true` returns \
             the imported source it came from.",
            &[
                "remind_me_get",
                "remind_me_history",
                "remind_me_source",
                "remind_me_get_capture",
            ],
            &[],
            json!({
                "capture_id": { "type": "string", "description": "The shared id of a dialog and summary saved by remind_me_auto_capture." },
                "history": { "type": "boolean", "default": false, "description": "List the memory's earlier versions, newest first." },
                "raw": { "type": "boolean", "default": false, "description": "Return the raw imported bytes the memory came from." }
            }),
        ),
        "remind_me_update" => merged(
            "Edit a remind-me memory's content, category, tags, metadata or type. Pass \
             `remind_at` on its own to set a reminder, or null to clear one.",
            &["remind_me_update", "remind_me_set_reminder"],
            &[],
            json!({}),
        ),
        "remind_me_digest" => merged(
            "Summarise the remind-me vault: memories added recently and its current health. \
             `section=stats` gives counts by category, `section=vitality` the decay report.",
            &[
                "remind_me_digest",
                "remind_me_stats",
                "remind_me_vitality_report",
            ],
            &[],
            json!({ "section": choice(DIGEST, Some("recent")) }),
        ),
        "remind_me_server_status" => merged(
            "Report remind-me's own state: where the data lives, the schema version, which \
             subsystems run and which tool profile is active. `section` reports the folder \
             watcher, the webhook, the update check or the import connectors instead.",
            &["remind_me_server_status"],
            &[],
            json!({ "section": choice(STATUS, Some("overview")) }),
        ),
        "remind_me_queue" => merged(
            "remind-me pipeline: fetch the next memories waiting for a refinement step (`kind`). \
             Process them, then send the results to the matching tool: decompose, annotate, \
             normalize_apply, reclassify or promote.",
            &handlers(QUEUE),
            &["batch_size"],
            json!({
                "kind": choice(QUEUE, None),
                "limit": { "type": "integer", "minimum": 1, "description": "How many to fetch." }
            }),
        ),
        "remind_me_wiki_read" => merged(
            "remind-me wiki: read one page by `slug`, search pages with `query`, load the whole \
             wiki with `all=true`, or list every page when given nothing.",
            &[
                "remind_me_wiki_read",
                "remind_me_wiki_search",
                "remind_me_wiki_load",
                "remind_me_wiki_list",
            ],
            &[],
            json!({ "all": { "type": "boolean", "default": false, "description": "Load every page as one document, up to token_budget." } }),
        ),
        "remind_me_import" => merged(
            "remind-me import: load memories from a file, a directory, a dbs archive, a MemPalace \
             store or a folder of Markdown wiki pages (`from`). Imports deduplicate; \
             remind_me_undo_import rolls one back.",
            &handlers(IMPORT_FROM),
            &["file_path", "directory", "db_path", "dir"],
            json!({
                "from": choice(IMPORT_FROM, Some("file")),
                "path": { "type": "string", "description": "The file, directory or archive to read. MemPalace reads its configured store." }
            }),
        ),
        "remind_me_reminders" => merged(
            "remind-me extras: list memories with a reminder set, soonest first, or return the \
             calendar feed URL with `ics_url=true`. Set one with remind_me_update `remind_at`.",
            &["remind_me_list_reminders", "remind_me_reminders_ics_url"],
            &[],
            json!({ "ics_url": { "type": "boolean", "default": false, "description": "Return the subscribable ICS feed URL. It embeds a secret token." } }),
        ),
        "remind_me_saved_search" => merged(
            "remind-me extras: keep a saved query by name. `action=save` stores one, `list` shows \
             them, `run` re-runs one and `delete` removes it.",
            &handlers(SAVED_SEARCH),
            &[],
            json!({ "action": choice(SAVED_SEARCH, None) }),
        ),
        "remind_me_sync_status" => merged(
            "remind-me admin: report multi-node sync state. `reconcile=hub`, or `reconcile=peer` \
             with `node_id`, diffs record counts against the hub or one peer and says why they \
             drift.",
            &handlers(SYNC),
            &[],
            json!({ "reconcile": choice(SYNC, Some("none")) }),
        ),
        _ => catalog.iter().find(|t| t["name"] == name).cloned(),
    }
}

fn handlers(choices: Choices) -> Vec<&'static str> {
    choices.iter().map(|(_, handler)| *handler).collect()
}

/// A string-enum property over `choices`' names.
fn choice(choices: Choices, default: Option<&str>) -> Value {
    let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
    let mut property = json!({ "type": "string", "enum": names });
    if let Some(default) = default {
        property["default"] = json!(default);
    }
    property
}

/// One tool whose properties are the union of `handlers`' (the first to
/// declare a property wins), minus `drop`, plus `extra`. `required` is the
/// first handler's, kept only where every handler requires it, since a
/// merged call may reach any of them.
///
/// A property whose handlers disagree on its default (`response_format` is
/// Markdown for `digest` but JSON for `vitality_report`) is advertised with
/// no default: each handler keeps its own, and one advertised value would be
/// wrong for some of them.
fn merge(
    name: &str,
    description: &str,
    catalog: &[Value],
    handlers: &[&str],
    drop: &[&str],
    extra: Value,
) -> Value {
    let schemas: Vec<&Value> = handlers
        .iter()
        .filter_map(|h| catalog.iter().find(|t| t["name"] == *h))
        .map(|t| &t["inputSchema"])
        .collect();
    let mut properties = Map::new();
    let mut disputed = BTreeSet::new();
    for schema in &schemas {
        for (key, value) in schema["properties"].as_object().into_iter().flatten() {
            if drop.contains(&key.as_str()) {
                continue;
            }
            match properties.get(key) {
                None => {
                    properties.insert(key.clone(), value.clone());
                }
                Some(seen) if seen.get("default") != value.get("default") => {
                    disputed.insert(key.clone());
                }
                Some(_) => {}
            }
        }
    }
    for key in &disputed {
        if let Some(property) = properties.get_mut(key).and_then(Value::as_object_mut) {
            property.remove("default");
        }
    }
    for (key, value) in extra.as_object().into_iter().flatten() {
        properties.insert(key.clone(), value.clone());
    }
    let required_by = |schema: &&Value, key: &Value| {
        schema["required"]
            .as_array()
            .is_some_and(|r| r.contains(key))
    };
    let required: Vec<Value> = schemas
        .first()
        .and_then(|s| s["required"].as_array())
        .into_iter()
        .flatten()
        .filter(|key| schemas.iter().all(|s| required_by(s, key)))
        .cloned()
        .collect();
    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    json!({ "name": name, "description": description, "inputSchema": schema })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(tool: &str, args: Value) -> (String, Value) {
        let route = route(tool, args).expect("routes");
        (route.handler, route.args)
    }

    #[test]
    fn search_without_a_query_lists_and_with_a_reference_looks_it_up() {
        assert_eq!(
            routed("remind_me_search", json!({"query": "x"})).0,
            "remind_me_search"
        );
        assert_eq!(
            routed("remind_me_search", json!({"query": " "})).0,
            "remind_me_list"
        );
        assert_eq!(routed("remind_me_search", json!({})).0, "remind_me_list");
        for shaped in [
            json!({"detail": "full"}),
            json!({"query": "", "token_budget": 50}),
            json!({"min_confidence": 0.5}),
            json!({"include_expired": true}),
        ] {
            let why = route("remind_me_search", shaped.clone()).unwrap_err();
            assert!(why.contains("without `query`"), "{shaped}: {why}");
        }
        let (handler, args) = routed(
            "remind_me_search",
            json!({"query": " ", "category": "fact", "detail": null}),
        );
        assert_eq!(handler, "remind_me_list");
        assert!(args.get("query").is_none(), "{args}");
        let (handler, args) = routed(
            "remind_me_search",
            json!({"reference": "o/r#1", "reference_kind": "issue"}),
        );
        assert_eq!(handler, "remind_me_references");
        assert_eq!(args, json!({"value": "o/r#1", "kind": "issue"}));
    }

    #[test]
    fn a_reference_lookup_refuses_search_filters_it_cannot_honour() {
        let err = route(
            "remind_me_search",
            json!({"reference": "o/r#1", "project": "p", "category": "c"}),
        )
        .unwrap_err();
        assert!(err.contains("project") && err.contains("category"), "{err}");
        for shaping in [
            json!({"include_expired": false}),
            json!({"limit": 5}),
            json!({"response_format": "markdown"}),
        ] {
            let mut args = json!({"reference": "o/r#1"});
            args.as_object_mut()
                .unwrap()
                .extend(shaping.as_object().unwrap().clone());
            assert!(
                route("remind_me_search", args).is_err(),
                "{shaping} not refused"
            );
        }
        // Null is unset, and include_sensitive is honoured by the lookup.
        let ok = json!({"reference": "o/r#1", "project": null, "include_sensitive": true});
        assert_eq!(routed("remind_me_search", ok).0, "remind_me_references");
    }

    #[test]
    fn get_routes_by_capture_history_and_raw() {
        assert_eq!(
            routed("remind_me_get", json!({"id": "m"})).0,
            "remind_me_get"
        );
        assert_eq!(
            routed("remind_me_get", json!({"capture_id": "c"})).0,
            "remind_me_get_capture"
        );
        let (handler, args) = routed("remind_me_get", json!({"id": "m", "history": true}));
        assert_eq!(handler, "remind_me_history");
        assert_eq!(args["memory_id"], "m");
        assert_eq!(
            routed("remind_me_get", json!({"memory_id": "m", "raw": true})).0,
            "remind_me_source"
        );
        assert!(route("remind_me_get", json!({"history": true, "raw": true})).is_err());
        assert_eq!(
            routed("remind_me_get", json!({"memory_ids": ["a", "b"]})).0,
            "remind_me_get"
        );
        assert!(route(
            "remind_me_get",
            json!({"memory_ids": ["a"], "history": true})
        )
        .is_err());
        assert!(route(
            "remind_me_get",
            json!({"memory_ids": ["a"], "capture_id": "c"})
        )
        .is_err());
    }

    #[test]
    fn a_reminder_is_set_alone_or_refused() {
        assert_eq!(
            routed(
                "remind_me_update",
                json!({"memory_id": "m", "content": "c"})
            )
            .0,
            "remind_me_update"
        );
        assert_eq!(
            routed(
                "remind_me_update",
                json!({"memory_id": "m", "remind_at": null})
            )
            .0,
            "remind_me_set_reminder"
        );
        let err = route(
            "remind_me_update",
            json!({"memory_id": "m", "remind_at": "2030-01-01T00:00:00Z", "tags": []}),
        )
        .unwrap_err();
        assert!(err.contains("tags"), "{err}");
    }

    #[test]
    fn queue_needs_a_kind_and_maps_limit_for_batch_fetchers() {
        let err = route("remind_me_queue", json!({})).unwrap_err();
        assert!(err.contains("decompose"), "names the kinds: {err}");
        assert!(route("remind_me_queue", json!({"kind": "nope"})).is_err());
        let (handler, args) = routed("remind_me_queue", json!({"kind": "extract", "limit": 5}));
        assert_eq!(handler, "remind_me_extract_batch");
        assert_eq!(args["batch_size"], 5);
        let (handler, args) = routed("remind_me_queue", json!({"kind": "stale", "limit": 5}));
        assert_eq!(handler, "remind_me_stale_candidates");
        assert_eq!(args["limit"], 5);
    }

    #[test]
    fn import_maps_path_to_each_handlers_own_name() {
        let cases = [
            ("file", "remind_me_import_chat", "file_path"),
            ("directory", "remind_me_import_directory", "directory"),
            ("dbs", "remind_me_import_dbs", "db_path"),
            ("wiki", "remind_me_wiki_import", "dir"),
        ];
        for (from, handler, key) in cases {
            let (h, args) = routed("remind_me_import", json!({"from": from, "path": "/p"}));
            assert_eq!(h, handler);
            assert_eq!(args[key], "/p", "{from}");
        }
        assert_eq!(
            routed("remind_me_import", json!({"path": "/p"})).0,
            "remind_me_import_chat"
        );
    }

    #[test]
    fn the_remaining_merges_route_by_their_discriminator() {
        let cases = [
            ("remind_me_digest", json!({}), "remind_me_digest"),
            (
                "remind_me_digest",
                json!({"section": "stats"}),
                "remind_me_stats",
            ),
            (
                "remind_me_server_status",
                json!({}),
                "remind_me_server_status",
            ),
            (
                "remind_me_server_status",
                json!({"section": "webhook"}),
                "remind_me_webhook_status",
            ),
            ("remind_me_wiki_read", json!({}), "remind_me_wiki_list"),
            (
                "remind_me_wiki_read",
                json!({"slug": "s"}),
                "remind_me_wiki_read",
            ),
            (
                "remind_me_wiki_read",
                json!({"query": "q"}),
                "remind_me_wiki_search",
            ),
            (
                "remind_me_wiki_read",
                json!({"all": true}),
                "remind_me_wiki_load",
            ),
            ("remind_me_reminders", json!({}), "remind_me_list_reminders"),
            (
                "remind_me_reminders",
                json!({"ics_url": true}),
                "remind_me_reminders_ics_url",
            ),
            (
                "remind_me_saved_search",
                json!({"action": "run"}),
                "remind_me_run_saved_search",
            ),
            ("remind_me_sync_status", json!({}), "remind_me_sync_status"),
            (
                "remind_me_sync_status",
                json!({"reconcile": "peer"}),
                "remind_me_sync_reconcile_peer",
            ),
        ];
        for (tool, args, handler) in cases {
            assert_eq!(routed(tool, args.clone()).0, handler, "{tool} {args}");
        }
        assert!(route("remind_me_saved_search", json!({})).is_err());
    }

    #[test]
    fn an_alias_reaches_its_own_handler_with_its_own_arguments() {
        let args = json!({"batch_size": 3});
        assert_eq!(
            routed("remind_me_decompose_batch", args.clone()),
            ("remind_me_decompose_batch".to_string(), args)
        );
    }

    fn tool(name: &str, properties: Value, required: Value) -> Value {
        json!({ "name": name, "description": "", "inputSchema": {
            "type": "object", "properties": properties, "required": required } })
    }

    #[test]
    fn a_merged_schema_unions_drops_and_requires_only_what_every_handler_requires() {
        let catalog = [
            tool(
                "remind_me_import_chat",
                json!({"file_path": {}, "kind": {}}),
                json!(["file_path"]),
            ),
            tool(
                "remind_me_import_dbs",
                json!({"db_path": {}, "source": {}}),
                json!([]),
            ),
            tool(
                "remind_me_update",
                json!({"memory_id": {}, "content": {}}),
                json!(["memory_id"]),
            ),
            tool(
                "remind_me_set_reminder",
                json!({"memory_id": {}, "remind_at": {}}),
                json!(["memory_id"]),
            ),
            tool(
                "remind_me_digest",
                json!({"response_format": {"default": "markdown"}}),
                json!([]),
            ),
            tool(
                "remind_me_stats",
                json!({"response_format": {"default": "markdown"}}),
                json!([]),
            ),
            tool(
                "remind_me_vitality_report",
                json!({"response_format": {"default": "json"}}),
                json!([]),
            ),
        ];
        let import = merge(
            "remind_me_import",
            "d",
            &catalog,
            &["remind_me_import_chat", "remind_me_import_dbs"],
            &["file_path", "db_path"],
            json!({"path": {"type": "string"}}),
        );
        let props = import["inputSchema"]["properties"].as_object().unwrap();
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(keys, ["kind", "path", "source"]);
        assert!(import["inputSchema"].get("required").is_none());

        let update = merge(
            "remind_me_update",
            "d",
            &catalog,
            &["remind_me_update", "remind_me_set_reminder"],
            &[],
            json!({}),
        );
        assert_eq!(update["inputSchema"]["required"], json!(["memory_id"]));
        assert!(update["inputSchema"]["properties"]["remind_at"].is_object());

        let agreed = merge(
            "remind_me_digest",
            "d",
            &catalog,
            &["remind_me_digest", "remind_me_stats"],
            &[],
            json!({}),
        );
        assert_eq!(
            agreed["inputSchema"]["properties"]["response_format"]["default"],
            "markdown"
        );
        let disputed = merge(
            "remind_me_digest",
            "d",
            &catalog,
            &["remind_me_digest", "remind_me_vitality_report"],
            &[],
            json!({}),
        );
        assert!(disputed["inputSchema"]["properties"]["response_format"]
            .get("default")
            .is_none());
    }
}
