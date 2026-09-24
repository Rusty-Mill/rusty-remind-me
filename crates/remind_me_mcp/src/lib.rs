// The `tools/list` response is one `json!` literal covering every tool, and
// `json!` expands recursively — so the limit is reached by the *number* of
// tools as well as by any one schema's nesting depth. Extracting a schema into
// its own function (see `annotate_input_schema`) fixes the depth case; nothing
// but a higher limit fixes the breadth case short of building the array as a
// `Vec<Value>` from several smaller literals. Raise this when it bites again,
// or do that restructuring — the literal is around thirty tools now, and the
// restructuring is the better answer once it is closer to sixty.
#![recursion_limit = "512"]

pub mod render;

/// The response format a call asked for, defaulting to **JSON** (#206).
///
/// Read from the raw arguments rather than added as a field to twelve separate
/// input models — four of those tools have no input model at all, so there is
/// nowhere to put it, and the remaining eight would each need a bespoke
/// `#[serde(default)]` that disagreed with `ResponseFormat`'s own crate-wide
/// Markdown default.
///
/// JSON is the default because JSON is what these twelve already returned.
/// Every existing caller keeps working and Markdown is purely additive; the
/// reference's own default is Markdown, so the *defaults* still differ, but
/// flipping this would break every current caller in order to imitate a
/// limitation.
///
/// Anything unrecognised falls through to JSON rather than erroring: an
/// unknown format is a caller mistake that should still return their data,
/// not a failed call.
///
/// Tools that already carry `response_format` in their own input model —
/// `remind_me_history`, `remind_me_list`, `remind_me_search` — keep parsing it
/// there, with their existing defaults. This value is only consulted by arms
/// that had no choice before.
/// Selects the fallback format for the tools that have no reference-mandated
/// one. `json` (the default) or `markdown`.
pub const DEFAULT_FORMAT_ENV: &str = "REMIND_ME_DEFAULT_RESPONSE_FORMAT";

fn requested_format(args: &serde_json::Value) -> ResponseFormat {
    format_or(args, configured_default_format())
}

/// The configured fallback for the [`requested_format`] population.
fn configured_default_format() -> ResponseFormat {
    default_format_from(std::env::var(DEFAULT_FORMAT_ENV).ok().as_deref())
}

/// [`configured_default_format`] with the raw variable injected.
///
/// Separate so the parsing can be tested without `set_var`, which is
/// process-global and races every other test in the binary.
///
/// # Why this moves twelve tools and not sixty-three
///
/// After #224 this port matches the reference's default for every tool that
/// mirrors a reference input model — Markdown for `search`, `list`,
/// `wiki_list`, `stats`, `history`, `digest` and `list_reminders`, JSON for
/// `vitality_report`. Those defaults are *fixed by the reference* and this
/// variable deliberately does not touch them: making `vitality_report` render
/// Markdown because someone asked for "markdown defaults" would move the port
/// away from the reference, which is the opposite of the point.
///
/// What remains is the twelve tools from #211, for which the reference has no
/// `response_format` at all — it returns Markdown and offers no JSON. The port
/// added the parameter as a pure addition and defaulted it to JSON so existing
/// callers were unaffected (#206). That choice is right for this port's own
/// callers and wrong for anyone substituting this binary into a client
/// configured against `remind_me`, and one default cannot serve both.
///
/// So: unset leaves every byte as it is today; `markdown` makes those twelve
/// match the reference, which is the last thing standing between this and a
/// drop-in MCP server (#226).
fn default_format_from(raw: Option<&str>) -> ResponseFormat {
    match raw.map(str::trim) {
        // Case-insensitive because this arrives from a shell or a JSON config
        // by hand, where `Markdown` is at least as likely as `markdown`.
        Some(v) if v.eq_ignore_ascii_case("markdown") => ResponseFormat::Markdown,
        // Everything else -- unset, blank, `json`, or a typo -- is JSON. A
        // misspelled value silently selecting Markdown would be a worse
        // failure than one silently selecting the documented default.
        _ => ResponseFormat::Json,
    }
}

/// Read `response_format` from raw arguments, falling back to `default`.
///
/// The fallback is a parameter rather than a constant because the right default
/// is per-tool, not global (#224). Two populations, and they disagree for good
/// reasons:
///
/// - Tools that mirror a reference model take **that model's** default. The
///   reference sets MARKDOWN on seven of its eight `response_format` fields and
///   JSON on `VitalityReportInput`; matching per-tool is what makes an MCP
///   client configured against `remind_me` behave the same way here.
/// - The twelve tools from #211 take **JSON**, via [`requested_format`]. The
///   reference has no `response_format` field for those at all — it returns
///   Markdown and offers no JSON — so the parameter is a pure addition here and
///   JSON keeps every existing caller of this port unaffected (#206).
///
/// An unrecognised value falls through to `default` rather than erroring, which
/// is what `requested_format` has always done for unknown strings.
fn format_or(args: &serde_json::Value, default: ResponseFormat) -> ResponseFormat {
    match args.get("response_format").and_then(|v| v.as_str()) {
        Some("markdown") => ResponseFormat::Markdown,
        Some("json") => ResponseFormat::Json,
        _ => default,
    }
}

use remind_me_core::{
    backup, capture, consolidation::consolidate, contradictions, db::queries, dbs_import, digest,
    entity, export, history, importer, mempalace_import, normalize, recalibrate, saved_searches,
    stats, status, sync::SyncPeer, undo_import, updater, vectors, vitality, watcher,
    webhook::Webhook, wiki, wiki_fs::Wiki, wiki_import, AnnotateInput, AutoCaptureInput,
    BulkImportDirInput, ChatImportInput, ConsolidateInput, ContradictionCandidatesInput, Database,
    DbsImportInput, DecomposeBatchInput, DecomposeInput, DigestInput, EntityInput,
    EntityLookupInput, EntityTraverseInput, ExportInput, ExtractBatchInput, FeedbackInput,
    HistoryInput, ListRemindersInput, MemoryAddInput, MemoryListInput, MemorySearchInput,
    MemoryStatsInput, MemoryUpdateInput, MempalaceImportInput, NormalizeApplyInput,
    NormalizeBatchInput, RecalibrateCandidatesInput, ReclassifyBatchInput, ReclassifyInput,
    ReconcilePeerInput, ResponseFormat, RevertInput, SaveSearchInput, SavedSearchNameInput,
    SetReminderInput, SyncRepairInput, UndoImportInput, UpdateOutcome, WikiDeleteOutcome,
    ANNOTATE_BATCH_MAX, ANNOTATE_BATCH_MIN, CONSOLIDATE_LIMIT_MAX, CONSOLIDATE_LIMIT_MIN,
    CONSOLIDATE_SIMILARITY_MAX, CONSOLIDATE_SIMILARITY_MIN, CONTRADICTION_LIMIT_MAX,
    CONTRADICTION_LIMIT_MIN, DBS_IMPORT_LIMIT_MAX, DBS_IMPORT_LIMIT_MIN, DECOMPOSE_BATCH_MAX,
    DECOMPOSE_BATCH_MIN, DECOMPOSE_FACTS_MAX, DECOMPOSE_FACTS_MIN, ENTITY_LOOKUP_LIMIT_MAX,
    ENTITY_LOOKUP_LIMIT_MIN, EXTRACT_BATCH_MAX, EXTRACT_BATCH_MIN, EXTRACT_MODES,
    HISTORY_LIMIT_MAX, HISTORY_LIMIT_MIN, IMPORT_MAX_LENGTH_MAX, IMPORT_MAX_LENGTH_MIN,
    MEMPALACE_IMPORT_LIMIT_MAX, MEMPALACE_IMPORT_LIMIT_MIN, NORMALIZE_APPLY_MAX,
    NORMALIZE_APPLY_MIN, NORMALIZE_BATCH_MAX, NORMALIZE_BATCH_MIN, PROMOTION_LIMIT_DEFAULT,
    PROMOTION_LIMIT_MAX, PROMOTION_LIMIT_MIN, RECALIBRATE_LIMIT_MAX, RECALIBRATE_LIMIT_MIN,
    RECLASSIFY_BATCH_MAX, RECLASSIFY_BATCH_MIN, REMINDER_LIMIT_MAX, REMINDER_LIMIT_MIN,
    STALE_CANDIDATES_LIMIT_DEFAULT, STALE_CANDIDATES_LIMIT_MAX, STALE_CANDIDATES_LIMIT_MIN,
    UNDO_IMPORT_LIMIT_MAX, UNDO_IMPORT_LIMIT_MIN,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

/// Input schema for `remind_me_annotate`.
///
/// Built here rather than inline in the `tools/list` literal: that literal is
/// one `json!` invocation covering every tool, and this schema's nesting depth
/// pushed the macro past its expansion recursion limit. Interpolating an
/// already-built `Value` costs no expansion depth. Any further deeply-nested
/// schema should be extracted the same way.
fn annotate_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let annotation = json!({
        "type": "object",
        "properties": {
            "memory_id": { "type": "string" },
            "subject": { "type": "string" },
            "predicate": { "type": "string" },
            "object": { "type": "string" },
            "entities": { "type": "array", "items": entity }
        },
        "required": ["memory_id"]
    });

    json!({
        "type": "object",
        "properties": {
            "annotations": {
                "type": "array",
                "minItems": ANNOTATE_BATCH_MIN,
                "maxItems": ANNOTATE_BATCH_MAX,
                "items": annotation
            }
        },
        "required": ["annotations"]
    })
}

/// Input schema for `remind_me_normalize_apply`.
///
/// Extracted for the same reason as [`annotate_input_schema`] — nesting an
/// entity array inside an entry array inside the batch pushes the shared
/// `json!` literal past its expansion recursion limit.
fn normalize_apply_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let entry = json!({
        "type": "object",
        "properties": {
            "memory_id": { "type": "string", "description": "The raw import being distilled" },
            "question": { "type": "string", "maxLength": 500 },
            "summary": { "type": "string", "maxLength": 10000 },
            "resolution": { "type": "string", "maxLength": 5000 },
            "refs": { "type": "array", "items": { "type": "string" }, "maxItems": 20 },
            "entities": { "type": "array", "items": entity, "maxItems": 20 }
        },
        "required": ["memory_id", "question", "summary"]
    });

    json!({
        "type": "object",
        "properties": {
            "normalizations": {
                "type": "array",
                "minItems": NORMALIZE_APPLY_MIN,
                "maxItems": NORMALIZE_APPLY_MAX,
                "items": entry
            }
        },
        "required": ["normalizations"]
    })
}

/// Input schema for `remind_me_decompose`.
///
/// Extracted for the same reason as [`annotate_input_schema`]: an entity array
/// nested inside a fact array inside the batch exceeds the shared `json!`
/// literal's expansion recursion limit.
fn decompose_input_schema() -> Value {
    let entity = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string" },
            "aliases": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name"]
    });

    let fact = json!({
        "type": "object",
        "properties": {
            "content": { "type": "string", "minLength": 1, "maxLength": 50000 },
            "memory_type": { "type": "string", "description": "decision, preference, fact, insight, learning, blocker or action_item; defaults to unclassified" },
            "extra_tags": { "type": "array", "items": { "type": "string" }, "description": "Merged with the parent capture's tags" },
            "subject": { "type": "string", "maxLength": 200 },
            "predicate": { "type": "string", "maxLength": 200 },
            "object": { "type": "string", "maxLength": 500 },
            "entities": { "type": "array", "items": entity, "maxItems": 20 }
        },
        "required": ["content"]
    });

    json!({
        "type": "object",
        "properties": {
            "capture_id": { "type": "string", "minLength": 1 },
            "facts": {
                "type": "array",
                "minItems": DECOMPOSE_FACTS_MIN,
                "maxItems": DECOMPOSE_FACTS_MAX,
                "items": fact
            }
        },
        "required": ["capture_id", "facts"]
    })
}

/// Input schema shared by the two import tools.
///
/// `directory` takes a folder and a `recursive` flag; `file_path` takes one
/// file. Everything else is identical, so the shape is built once.
fn import_input_schema(directory: bool) -> Value {
    let mut properties = json!({
        "category": { "type": "string", "default": "chat_import", "description": "Category for imported memories. A document import replaces the chat default with 'document'." },
        "tags": { "type": "array", "items": { "type": "string" } },
        "extract_mode": {
            "type": "string",
            "enum": EXTRACT_MODES,
            "default": "assistant_messages",
            "description": "Which turns to keep from a chat export"
        },
        "max_length": { "type": "integer", "default": 10000, "minimum": IMPORT_MAX_LENGTH_MIN, "maximum": IMPORT_MAX_LENGTH_MAX, "description": "Characters per memory; longer content is chunked" },
        "kind": {
            "type": "string",
            // `obsidian` and `readwise` are reachable only by naming them.
            // `auto` deliberately never selects either: a Readwise export and
            // a chat export are both an unadorned `.json`, and guessing wrong
            // silently corrupts a working chat import.
            "enum": ["auto", "chat", "document", "obsidian", "readwise", "pdf", "image", "audio"],
            "default": "auto",
            "description": "auto/chat/document sniff by content; pdf, image (.png/.jpg/.jpeg, OCR) and audio (.mp3/.m4a/.wav/.ogg, transcription) are picked automatically from the extension and need the `pdf`, `ocr` and `audio` build features respectively; obsidian (.md, frontmatter + [[wikilinks]] + #tags) and readwise (.json export, one memory per highlight) must be named explicitly"
        }
    });
    let object = properties.as_object_mut().expect("just built an object");
    if directory {
        object.insert("directory".into(), json!({ "type": "string" }));
        object.insert(
            "recursive".into(),
            json!({ "type": "boolean", "default": true, "description": "Search subdirectories" }),
        );
    } else {
        object.insert("file_path".into(), json!({ "type": "string" }));
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": [if directory { "directory" } else { "file_path" }]
    })
}

pub struct McpServer {
    /// Declared **before** `db`, and that ordering is load-bearing: Rust drops
    /// struct fields in declaration order, `Webhook`'s drop joins its serving
    /// thread, and that thread writes through the database. Listed after `db`
    /// it would still be sound — the thread holds its own `Arc` — but the
    /// connections would stay open until the thread noticed, which is the
    /// shutdown ordering `SE-07` exists to pin down. `sync_peer` holds its
    /// own serving thread for the same reason and is declared before `db` for
    /// the same reason.
    ///
    /// No `sync_worker` field here (unlike before this comment last changed):
    /// `SyncWorker` is a background loop like the scheduler/watcher/promotion
    /// nudge, none of which this struct owns either — `remind_me_cli::main`
    /// starts all of them alongside, not through, an `McpServer`. `remind_me_
    /// server_status` reads a live one via `remind_me_core::sync::sync_live_
    /// status`, which answers for whichever process is actually running it,
    /// not only one this instance happens to hold.
    webhook: Webhook,
    sync_peer: SyncPeer,
    db: Arc<Database>,
    wiki: Wiki,
}

/// `"name/version"` from an MCP `initialize`, or `None` if it said nothing
/// useful.
///
/// The version is appended when present because "which agent" and "which
/// build of it" are different questions, and a provenance field that answers
/// only the first goes stale the moment the interesting difference is between
/// two releases of the same client.
///
/// A blank or absent name yields `None` rather than an empty string, so
/// `configured_client` falls through to the configured value instead of
/// recording a client called "".
fn client_identity(req: &serde_json::Value) -> Option<String> {
    let info = req.get("params")?.get("clientInfo")?;
    let name = info.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    match info.get("version").and_then(|v| v.as_str()).map(str::trim) {
        Some(version) if !version.is_empty() => Some(format!("{name}/{version}")),
        _ => Some(name.to_string()),
    }
}

impl McpServer {
    pub fn new(db: Database) -> Self {
        Self::with_wiki(db, Wiki::from_env())
    }

    /// Build a server against a specific wiki directory.
    ///
    /// Tests need this: the default root is a real shared directory, so a test
    /// using it would write into whatever wiki the machine's user actually has.
    pub fn with_wiki(db: Database, wiki: Wiki) -> Self {
        let db = Arc::new(db);
        Self {
            // A no-op without `REMIND_ME_WEBHOOK_SECRET`, which is the ordinary
            // case — nothing binds a port unless someone asked for one.
            webhook: Webhook::from_env(Arc::clone(&db)),
            // A no-op without `REMIND_ME_SYNC_SECRET` — accepting another
            // node's push/pull is off by default exactly like the webhook.
            sync_peer: SyncPeer::from_env(Arc::clone(&db)),
            db,
            wiki,
        }
    }

    /// Stop the push endpoint without tearing the server down.
    pub fn stop_webhook(&mut self) {
        self.webhook.stop();
    }

    /// The push endpoint's state, as `remind_me_webhook_status` reports it.
    pub fn webhook_status(&self) -> remind_me_core::webhook::WebhookStatus {
        self.webhook.status()
    }

    /// Answer one JSON-RPC request, or `None` for a notification.
    ///
    /// Every successful reply is stamped with `resultType: "complete"`
    /// before it leaves (see [`stamp_result_type`]) — a `2026-07-28`
    /// client requires it on every result, and this server has echoed
    /// back whatever `protocolVersion` a client asked for since the
    /// handshake was made dynamic, so it has been *claiming* that
    /// revision without meeting it. The one field is stamped here, at
    /// the single point every reply passes through, rather than at the
    /// several hundred `json!` sites that build them.
    pub fn handle_request(&self, request_json: &str) -> Option<Value> {
        let mut response = self.dispatch_request(request_json)?;
        stamp_result_type(&mut response);
        Some(response)
    }

    fn dispatch_request(&self, request_json: &str) -> Option<Value> {
        let req: Value = serde_json::from_str(request_json).ok()?;
        let method = req.get("method")?.as_str()?;
        let id = req.get("id").cloned();

        match method {
            "initialize" => {
                let req_id = id.unwrap_or(json!(1));
                let requested_version = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05");

                // The handshake carries who is calling, and this handler used
                // to read `protocolVersion` and drop the rest. Meanwhile every
                // memory recorded `client` from an environment variable
                // defaulting to "unknown" — the identity was on the wire and
                // being thrown away (#258).
                //
                // Advisory, not authentication: a client supplies its own name
                // and nothing verifies it. Good enough for "which agent wrote
                // this", which is the question `client` answers; not a basis
                // for any access decision, and this store has none anyway.
                remind_me_core::sync::set_handshake_client(client_identity(&req));

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": requested_version,
                        "capabilities": {
                            "tools": {
                                "listChanged": true
                            },
                            "resources": {
                                "listChanged": true,
                                "subscribe": true
                            },
                            "prompts": {
                                "listChanged": true
                            },
                            "logging": {}
                        },
                        "serverInfo": {
                            "name": "rusty_remind_me",
                            "version": "0.1.0"
                        }
                    }
                }))
            }
            "notifications/initialized" => None,
            "notifications/cancelled" => None,
            "ping" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {}
                }))
            }
            "tools/list" => {
                let req_id = id.unwrap_or(json!(1));
                let mut listed = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "tools": [
                            {
                                "name": "remind_me_add",
                                "description": "Store a new memory fact, preference, or note.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "content": { "type": "string" },
                                        "category": { "type": "string", "default": "general" },
                                        "tags": { "type": "array", "items": { "type": "string" } },
                                        "source": { "type": "string", "default": "manual" },
                                        "sensitive": { "type": "boolean", "default": false, "description": "Mark this memory sensitive: kept out of ordinary search and list results unless include_sensitive is set. A convenience flag, NOT access control — this is a single-user store and anyone with the database reads everything regardless." }
                                    },
                                    "required": ["content"]
                                }
                            },
                            {
                                "name": "remind_me_get",
                                "description": "Get a memory by ID.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" },
                                        "id": { "type": "string", "description": "Deprecated alias for memory_id. Prefer memory_id; kept for backward compatibility." }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_list",
                                "description": "List memories with optional filtering by category, tags, or source. Results are paginated, newest first.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown", "description": "markdown (default, as the reference) renders the memories; json returns the structured page." },
                                        "category": { "type": "string" },
                                        "tags": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Memory must have ALL of these tags"
                                        },
                                        "source": { "type": "string" },
                                        "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 },
                                        "offset": { "type": "integer", "default": 0, "minimum": 0 },
                                        "include_sensitive": { "type": "boolean", "default": false, "description": "Include memories marked sensitive. Off by default, so sensitive content never surfaces in an ordinary request." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_update",
                                "description": "Update an existing memory's content, category, tags, or metadata.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "memory_id": { "type": "string" },
                                        "content": { "type": "string" },
                                        "category": { "type": "string" },
                                        "tags": { "type": "array", "items": { "type": "string" } },
                                        "metadata": { "type": "object" },
                                        "sensitive": { "type": "boolean", "description": "Set or clear the sensitive flag. Omit to leave it unchanged." },
                                        "clear_superseded": { "type": "boolean", "default": false, "description": "Clear this memory's superseded_by flag, un-hiding it from search, entity, and subject/predicate lookups. Recovery path for a false-positive contradiction-supersession. Does not affect the memory that did the superseding." }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_delete",
                                "description": "Delete a memory by ID.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_search",
                                "description": "Search memories using FTS5 keyword & hybrid ranking. The three expansion flags surface adjacent memories in their own sections, outside the ranked results, so they never consume `limit`.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown", "description": "markdown (default, as the reference) renders the results; json returns the full envelope including scores." },
                                        "query": { "type": "string" },
                                        "limit": { "type": "integer", "default": 20 },
                                        "category": { "type": "string" },
                                        "include_dormant": { "type": "boolean", "default": false, "description": "Include memories that have decayed below the vitality floor" },
                                        "min_vitality": { "type": "number", "default": 0, "description": "Only return memories at or above this current vitality" },
                                        "expand_entities": { "type": "boolean", "default": false, "description": "Also surface memories mentioning the same entities" },
                                        "include_neighbors": { "type": "boolean", "default": false, "description": "Also surface adjacent chunks of the same source document" },
                                        "expand_co_retrieval": { "type": "boolean", "default": false, "description": "Also surface memories frequently retrieved alongside these" },
                                        "include_sensitive": { "type": "boolean", "default": false, "description": "Include memories marked sensitive. Off by default, so sensitive content never surfaces in an ordinary request." },
                                        "bootstrap": { "type": "boolean", "default": false, "description": "Prepend the durable persona (L3) as context, whether or not it matches the query. Spends up to a quarter of token_budget; the rest still goes to the ranked results." },
                                        "strategy": { "type": "string", "enum": ["auto", "balanced", "keyword_favored", "semantic_favored"], "default": "auto", "description": "RRF weight profile. Leave at auto (routes by query shape: quoted phrases, prefix* wildcards and very short queries favour keyword relevance; long or question-shaped queries favour semantic similarity) unless deliberately A/B testing a pinned preset." }
                                    },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "remind_me_entity",
                                "description": "Everything known about ONE named thing — a person, project, tool, org, or place. Read-only: returns the entity record, its facts, and the memories mentioning it, or found=false for an unknown name. Use remind_me_search for topic questions or anything you cannot name exactly.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string", "description": "Entity name or alias, resolved case- and whitespace-insensitively" },
                                        "limit": { "type": "integer", "default": 20, "minimum": ENTITY_LOOKUP_LIMIT_MIN, "maximum": ENTITY_LOOKUP_LIMIT_MAX, "description": "Maximum facts and maximum linked memories to return" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_entity_upsert",
                                "description": "Create a knowledge-graph entity, or update its kind. Not present in remind_me — remind_me_entity is read-only there, and is here too, so writing is a separate, explicit call.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "kind": { "type": "string" },
                                        "aliases": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_import_chat",
                                "description": "Import a chat export or document file into memory. .json/.jsonl are chat exports; .md/.markdown/.txt are content-sniffed in auto mode — chat role markers import as chat, everything else as a document chunked per section. Deduplicates by file content hash. The path must be inside the allowed import roots.",
                                "inputSchema": import_input_schema(false)
                            },
                            {
                                "name": "remind_me_import_directory",
                                "description": "Import every supported file in a directory. Same parsing and per-file dedup as remind_me_import_chat.",
                                "inputSchema": import_input_schema(true)
                            },
                            {
                                "name": "remind_me_import_dbs",
                                "description": "Bulk-import a daily-backup-system (dbs) archive. Reads its items/sources tables directly, read-only, and turns each live item into a memory with dbs's source and tags preserved as knowledge-graph entities rather than flattened into prose. Reruns are safe: unchanged items are skipped, and an item edited since its last import gets a fresh memory that supersedes the old one. Page a large archive with offset until has_more is false.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "db_path": { "type": "string", "description": "Path to the dbs SQLite archive, inside the allowed import roots" },
                                        "source": { "type": "string", "description": "Restrict to one dbs source name (e.g. 'raindrop'). Omit for all." },
                                        "item_type": { "type": "string", "description": "Restrict to one dbs item_kind (e.g. 'link'). Omit for all." },
                                        "limit": {
                                            "type": "integer",
                                            "minimum": DBS_IMPORT_LIMIT_MIN,
                                            "maximum": DBS_IMPORT_LIMIT_MAX,
                                            "default": 500
                                        },
                                        "offset": { "type": "integer", "minimum": 0, "default": 0 },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Extra tags added to every imported memory" },
                                        "dry_run": { "type": "boolean", "default": false, "description": "Report what would be imported without writing" }
                                    },
                                    "required": ["db_path"]
                                }
                            },
                            {
                                "name": "remind_me_import_mempalace",
                                "description": "Bulk-import memories from a MemPalace ChromaDB store, one page at a time. Reads its metadata segment directly, read-only, rather than one drawer at a time via MemPalace's own tools. A drawer carrying remind_me's own memory frontmatter has its category/tags/created restored; everything else is stored as one opaque memory per drawer, tagged with its wing and room. Already-imported drawers are skipped (tracked by drawer id), so reruns are safe. Reads REMIND_ME_MEMPALACE_PATH for the store location.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "wing": { "type": "string", "description": "Restrict to one wing (project). Omit for all." },
                                        "room": { "type": "string", "description": "Restrict to one room within the wing. Omit for all." },
                                        "limit": {
                                            "type": "integer",
                                            "minimum": MEMPALACE_IMPORT_LIMIT_MIN,
                                            "maximum": MEMPALACE_IMPORT_LIMIT_MAX,
                                            "default": 500
                                        },
                                        "offset": { "type": "integer", "minimum": 0, "default": 0 },
                                        "category": { "type": "string", "description": "Category for a drawer with no restorable frontmatter category (default: mempalace_import)" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Extra tags added to every imported memory" },
                                        "dry_run": { "type": "boolean", "default": false, "description": "Report what would be imported without writing" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_watch_status",
                                "description": "Report the folder watcher: which directories are watched, which were refused for sitting outside the import roots, scan counts, and recent errors. Says what to configure when nothing is.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_check_update",
                                "description": "Check whether this checkout is behind origin/main. Read-only: fetches from the remote and compares commits, never modifies anything.",
                                "inputSchema": { "type": "object", "properties": { "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." } } }
                            },
                            {
                                "name": "remind_me_self_update",
                                "description": "Pull the latest changes from origin/main (fast-forward only) and rebuild the workspace in release mode. Refuses a working tree with uncommitted changes unless force is set; force never bypasses the fast-forward-only pull, so a diverged local history is still refused either way. Always requires a restart to take effect on success.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "force": { "type": "boolean", "default": false, "description": "Skip the uncommitted-changes guard. Does not bypass the fast-forward-only pull." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_revoke_clients",
                                "description": "List OAuth clients registered with the remote connector's authorization server (FT-07), or revoke one by client_id. Without client_id, lists every registered client with its live access/refresh token counts -- this is the read path, not a bulk-revoke shorthand: there is no 'revoke all' operation, only 'list' (empty client_id) and 'revoke this one client' (client_id set). With client_id, deletes that client's registration and every token it holds; the live remote server re-reads the state file on each token check, so the client is locked out immediately and must re-register and re-obtain the owner's consent to reconnect.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "client_id": { "type": "string", "default": "", "description": "The client to revoke. Empty (default) lists clients instead of revoking anything." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_reindex",
                                "description": "Rebuild vector embeddings for every memory that doesn't have one yet. Existing embeddings are preserved; only missing ones are generated. Run this after configuring REMIND_ME_EMBEDDING_BACKEND, or after a bulk import that ran before an embedder was available. Reports 'degraded' when no embedder is configured or reachable, rather than silently doing nothing.",
                                "inputSchema": { "type": "object", "properties": { "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." } } }
                            },
                            {
                                "name": "remind_me_webhook_status",
                                "description": "Report the push ingestion endpoint: whether a secret is configured, whether it is listening, where, and how many pushes it has ingested, skipped or refused. Distinguishes 'nobody configured one' from 'configured but the port could not be bound'.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_list_connectors",
                                "description": "List every registered import connector — the pluggable parsers behind remind_me_import_chat's `kind` parameter.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_server_status",
                                "description": "Report where the data lives and what is running: database path and size, schema version against what this build expects, memory count, backup inventory, and which subsystems are active. Subsystems this crate does not implement are named with a reason rather than reported as stopped.",
                                "inputSchema": { "type": "object", "properties": { "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." } } }
                            },
                            {
                                "name": "remind_me_export_memories",
                                "description": "Export live memories to JSON or JSONL — every column, plus the entity graph. Deleted and superseded memories are excluded unless include_deleted is set, so an export is safe to re-import. Embedding vectors are excluded as derived data. Writes to file_path when given (must be inside the allowed export roots), otherwise returns the payload inline.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "format": { "type": "string", "enum": ["json", "jsonl"], "default": "json" },
                                        "category": { "type": "string", "description": "Filter: only export memories with this category" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Filter: memory must have ALL of these tags" },
                                        "file_path": { "type": "string", "description": "Destination file, inside the allowed export roots. Omit to return inline." },
                                        "include_graph": { "type": "boolean", "default": true, "description": "Append entities, links and relations as record_type-tagged records" },
                                        "include_deleted": { "type": "boolean", "default": false, "description": "Include soft-deleted and superseded memories. Off by default: exported records are read back as live content, so re-importing an export that carried them would resurrect deleted and stale memories. Set only for a genuine full-backup or audit export, not for moving memories between machines." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_decompose",
                                "description": "Break a captured conversation into individually searchable atomic facts, each linked to the capture. A fact whose subject/predicate match an existing fact but whose object differs supersedes it.",
                                "inputSchema": decompose_input_schema()
                            },
                            {
                                "name": "remind_me_decompose_batch",
                                "description": "Fetch captures that have not been decomposed into atomic facts yet.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": DECOMPOSE_BATCH_MIN, "maximum": DECOMPOSE_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_extract_batch",
                                "description": "Fetch memories that have no structured triple and no entity mentions yet, so they can be annotated with remind_me_annotate. Raw captured dialogs are excluded — their facts come out through remind_me_decompose instead.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": EXTRACT_BATCH_MIN, "maximum": EXTRACT_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_auto_capture",
                                "description": "Capture a whole conversation as two linked memories: the verbatim dialog and a concise summary. They share a capture_id, which remind_me_get_capture retrieves both by and remind_me_decompose breaks into atomic facts.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "conversation": { "type": "string", "minLength": 1, "maxLength": 500000, "description": "The verbatim exchange" },
                                        "summary": { "type": "string", "minLength": 1, "maxLength": 50000, "description": "A concise distillation of it" },
                                        "title": { "type": "string", "maxLength": 200, "description": "Defaults to the summary's first line" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 20 },
                                        "category": { "type": "string", "default": "conversation", "maxLength": 100, "description": "Category for the SUMMARY; the dialog is always stored as 'dialog'" },
                                        "metadata": { "type": "object" }
                                    },
                                    "required": ["conversation", "summary"]
                                }
                            },
                            {
                                "name": "remind_me_get_capture",
                                "description": "Retrieve a linked dialog and summary pair by their shared capture_id.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "capture_id": { "type": "string" }
                                    },
                                    "required": ["capture_id"]
                                }
                            },
                            {
                                "name": "remind_me_archive_prune",
                                "description": "Enforce the raw-transcript archive's retention limits (REMIND_ME_ARCHIVE_MAX_AGE_DAYS, REMIND_ME_ARCHIVE_MAX_BYTES), removing oldest-first. Runs automatically after each archived import; call it directly to reclaim space now or, with dry_run, to see what would go. Removes archives only — the memories derived from them are untouched.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "dry_run": { "type": "boolean", "default": true, "description": "Report what would be removed without removing it. Defaults to true — deletion is opt-in." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_promotion_candidates",
                                "description": "List what is ready to move up a rung of the refinement ladder (capture -> fact -> scenario -> persona). Each entry names the source memories, a snippet, and why it qualifies. Already-promoted sources are excluded, so re-running after promoting returns a shorter list rather than the same one.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "rung": { "type": "string", "enum": ["capture_to_fact", "fact_to_scenario", "scenario_to_persona"], "description": "capture_to_fact is reported here but promoted with remind_me_decompose." },
                                        "limit": { "type": "integer", "default": PROMOTION_LIMIT_DEFAULT, "minimum": PROMOTION_LIMIT_MIN, "maximum": PROMOTION_LIMIT_MAX }
                                    },
                                    "required": ["rung"]
                                }
                            },
                            {
                                "name": "remind_me_promote",
                                "description": "Store a distillation of some memories as the rung above them, recording what it was built from. Provenance is mandatory: a promoted artifact with no sources could never be checked or demoted. Refuses capture_to_fact — that is remind_me_decompose — and refuses a sensitive source for a persona.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "rung": { "type": "string", "enum": ["fact_to_scenario", "scenario_to_persona"] },
                                        "source_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "The memories this was distilled from" },
                                        "content": { "type": "string", "minLength": 1, "maxLength": 50000, "description": "The distillation" }
                                    },
                                    "required": ["rung", "source_ids", "content"]
                                }
                            },
                            {
                                "name": "remind_me_stale_candidates",
                                "description": "List memories whose content names a file (inside REMIND_ME_CODE_ROOTS) that has since changed or been deleted. Read-only and never touches the memory it reports: a changed file does not prove the memory wrong, it only means the claim is worth re-checking. Off entirely -- always an empty list -- unless REMIND_ME_CODE_ROOTS is configured.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "limit": { "type": "integer", "default": STALE_CANDIDATES_LIMIT_DEFAULT, "minimum": STALE_CANDIDATES_LIMIT_MIN, "maximum": STALE_CANDIDATES_LIMIT_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_persona",
                                "description": "The current persona: durable statements about the user whose grounds still hold. A statement whose sources have all been superseded or deleted is withheld automatically, so a contradicted fact demotes what was built on it. Set include_demoted to also see what is currently withheld and why.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "include_demoted": { "type": "boolean", "default": false, "description": "Also list statements currently withheld for lost provenance." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_provenance",
                                "description": "What a memory was distilled from, and what was distilled from it. Walks the refinement ladder in both directions from one memory id.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_skeleton_write",
                                "description": "Attach a Mermaid diagram of a capture's structure, so the conversation's shape can be read without reading the transcript. Each node maps to an inclusive, 1-based line range in the capture's dialog; remind_me_skeleton_read then drills into one node at a time. Ranges are validated against the dialog and the write is refused if any is out of bounds.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "capture_id": { "type": "string", "description": "The capture to describe" },
                                        "mermaid": { "type": "string", "minLength": 1, "maxLength": 100000, "description": "Mermaid source, e.g. 'graph TD\\n  n1[Ask] --> n2[Investigate]'" },
                                        "nodes": { "type": "object", "description": "Node id to [start_line, end_line], inclusive and 1-based, into the dialog. e.g. {\"n1\": [1, 12]}" }
                                    },
                                    "required": ["capture_id", "mermaid", "nodes"]
                                }
                            },
                            {
                                "name": "remind_me_skeleton_read",
                                "description": "Read a capture's skeleton. Without 'node' this returns the Mermaid diagram and its node map — the cheap view of a long conversation. With 'node' it returns only that node's lines of the dialog, which is how to drill in without pulling the whole transcript.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "capture_id": { "type": "string" },
                                        "node": { "type": "string", "description": "A node id from the diagram. Omit for the diagram itself." }
                                    },
                                    "required": ["capture_id"]
                                }
                            },
                            {
                                "name": "remind_me_source",
                                "description": "Retrieve the raw imported bytes a memory was derived from — the original transcript envelope, including the tool calls, reasoning blocks and session metadata that importing deliberately strips out. Only available for memories imported while REMIND_ME_ARCHIVE_DIR was set; returns nothing otherwise.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string", "description": "The memory whose source to fetch" },
                                        "include_sensitive": { "type": "boolean", "default": false, "description": "Return the source even when the memory is marked sensitive. The raw source discloses more than the memory does, so this is off by default." }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_normalize_batch",
                                "description": "Fetch raw imported memories (document/chat imports) that have not been normalized yet, so they can be distilled into a {question, summary, resolution?} shape. The raw memory is kept, not replaced.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": NORMALIZE_BATCH_MIN, "maximum": NORMALIZE_BATCH_MAX }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_sync_reconcile",
                                "description": "Diff this node's record counts against the hub's and classify the drift: in-sync, pull-lag (hub ahead, pull recent — the ordinary state), node-ahead (this node holds records the hub does not, so pushes are not landing — the direction that means data is at risk), or fault (hub ahead but the pull is stale or never ran). Read-only on both sides.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_sync_reconcile_peer",
                                "description": "The peer-to-peer counterpart of remind_me_sync_reconcile: diff this node's counts against one discovered peer's. Same four verdicts from the same classifier — 'local greater than remote means pushes are not landing' does not depend on which machine is on the other end.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "node_id": { "type": "string", "description": "Which discovered peer to reconcile against" }
                                    },
                                    "required": ["node_id"]
                                }
                            },
                            {
                                "name": "remind_me_api_key",
                                "description": "Issue, list, or revoke named dashboard API keys with a read or read-write scope. A read-scoped key is refused on every mutating HTTP route. The plaintext key is shown exactly once, at issuance — only its hash is stored, so it can be revoked and replaced but never recovered. Not multi-tenancy: every key reads and writes the same vault, only the permitted methods differ.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "action": { "type": "string", "enum": ["issue", "list", "revoke"], "default": "list" },
                                        "name": { "type": "string", "description": "Key name; required for issue and revoke" },
                                        "scope": { "type": "string", "enum": ["read", "read-write"], "default": "read", "description": "issue only" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_reminders_ics_url",
                                "description": "Return the subscribable ICS calendar feed URL for reminders. Paste it into a calendar app's \"subscribe by URL\" feature to see every upcoming and overdue-undelivered reminder as an event. WARNING: the URL embeds a secret token and whoever holds it can read every reminder's content — treat it exactly like a password. Rotate by deleting the token file.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_set_reminder",
                                "description": "Set a future reminder on an existing memory, or clear one already set. Pass an ISO-8601 timestamp for remind_at; omit it or pass null to clear. Naive timestamps are read as UTC. A timestamp already in the past is rejected rather than stored, because it could never fire.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "memory_id": { "type": "string", "description": "The memory to set or clear a reminder on" },
                                        "remind_at": { "type": ["string", "null"], "description": "ISO-8601 timestamp for when to surface this memory. Must be in the future. Omit or null to clear." }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_list_reminders",
                                "description": "List memories with a reminder set, soonest first. 'upcoming' is still in the future; 'overdue' came due but has not been delivered — typically because nothing was running when it fired; 'all' is both. A delivered reminder drops out of every window.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "when": { "type": "string", "enum": ["upcoming", "overdue", "all"], "default": "upcoming" },
                                        "limit": { "type": "integer", "default": 20, "minimum": REMINDER_LIMIT_MIN, "maximum": REMINDER_LIMIT_MAX },
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_sync_status",
                                "description": "Report multi-node sync state: outbox depth with a drain verdict, tombstone counts, and per-remote contact times. Liveness comes from wall-clock contact timestamps, not content cursors, so a quiet healthy remote is distinguishable from a wedged one. The first call establishes a drain baseline and reports 'unknown'; call again after ~30s for a rate.",
                                "inputSchema": { "type": "object", "properties": {} }
                            },
                            {
                                "name": "remind_me_sync_repair",
                                "description": "Reset a remote's pull cursors so the next sync re-pulls history from the beginning. Only the cursors are reset — the contact timestamps record what actually happened and are left intact.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "remote_id": { "type": "string", "default": "hub", "description": "Which remote to repair" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_digest",
                                "description": "A vault digest: memories added recently, and current vitality. Sensitive memories are always excluded, with no override — a digest is the ambient surface that flag exists to protect. Sections whose subsystem is not built yet are omitted rather than shown empty.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "since_days": { "type": "integer", "default": 7, "minimum": digest::DIGEST_SINCE_DAYS_MIN, "maximum": digest::DIGEST_SINCE_DAYS_MAX, "description": "How many days back counts as recent" },
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_contradiction_candidates",
                                "description": "Surface pairs of memories that might assert incompatible things but were never caught by exact-triple supersession — two pieces of prose that conflict without either carrying a formal subject/predicate/object. Read-only: these are pairs that MIGHT conflict, and most turn out merely topically similar. Read both before acting; fix a real one with remind_me_update, remind_me_delete, or remind_me_add carrying an explicit triple. Paginate by passing the next_after_a/next_after_b from one response as after_a/after_b on the next call, and stop when has_more is false — without a cursor every call returns the same first page, so a large queue has only `limit` reachable rows.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "limit": { "type": "integer", "default": 20, "minimum": CONTRADICTION_LIMIT_MIN, "maximum": CONTRADICTION_LIMIT_MAX },
                                        "after_a": { "type": "string", "description": "Keyset cursor: next_after_a from the previous response. Must be passed together with after_b." },
                                        "after_b": { "type": "string", "description": "Keyset cursor: next_after_b from the previous response. Must be passed together with after_a." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_history",
                                "description": "List a memory's edit history — snapshots of its content, category, tags, metadata and sensitive flag taken before each edit replaced them. Newest first. Revision ids from here are what remind_me_revert takes.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" },
                                        "limit": { "type": "integer", "default": 10, "minimum": HISTORY_LIMIT_MIN, "maximum": HISTORY_LIMIT_MAX },
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown" }
                                    },
                                    "required": ["memory_id"]
                                }
                            },
                            {
                                "name": "remind_me_revert",
                                "description": "Restore a memory's content, category, tags, metadata and sensitive flag to a prior revision. Reverting is itself an edit: it records a new revision of the state just before the revert, so a revert can be undone.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "memory_id": { "type": "string" },
                                        "revision_id": { "type": "integer", "description": "A revision id from remind_me_history for this same memory" },
                                        "reason": { "type": "string", "description": "Optional note recorded on the revision this revert creates" }
                                    },
                                    "required": ["memory_id", "revision_id"]
                                }
                            },
                            {
                                "name": "remind_me_save_search",
                                "description": "Save a query and its filters under a unique name, so a recurring question does not have to be retyped. Re-saving the same name updates it in place. Set watch=true to have polling report matches that have not been seen before.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "name": { "type": "string", "description": "Unique name for this saved search" },
                                        "query": { "type": "string", "description": "The search query to store and later re-run" },
                                        "category": { "type": "string", "description": "Optional category filter" },
                                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tag filter; a memory must have ALL of these" },
                                        "include_sensitive": { "type": "boolean", "default": false, "description": "Whether re-running this search includes memories marked sensitive" },
                                        "watch": { "type": "boolean", "default": false, "description": "Poll this search and report matches not seen before. Does not narrow what running it returns." }
                                    },
                                    "required": ["name", "query"]
                                }
                            },
                            {
                                "name": "remind_me_list_saved_searches",
                                "description": "List every saved search, alphabetical by name, with its stored query, filters and watch flag.",
                                "inputSchema": { "type": "object", "properties": { "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." } } }
                            },
                            {
                                "name": "remind_me_run_saved_search",
                                "description": "Re-run a saved search's stored query and filters. Returns all current matches — watching does not narrow this; the unseen-only diff belongs to polling.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string", "description": "Name of the saved search to run" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_delete_saved_search",
                                "description": "Delete a saved search by name, along with the seen-memory rows its watch tracking accumulated.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string", "description": "Name of the saved search to delete" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_undo_import",
                                "description": "Roll back a previous import, removing its memories and its tracking rows. Defaults to a dry run — pass dry_run=false to actually delete. On a sync-enabled node this soft-deletes (tombstones), so the removal propagates to every other node and disk is not reclaimed until compaction. Resumable: call again until 'remaining' is 0.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "import_kind": { "type": "string", "enum": ["chat", "dbs", "mempalace"], "description": "Which import to undo" },
                                        "import_id": { "type": "string", "description": "Scope to one import run. For 'chat' the chat_imports import_id; for 'dbs' the dbs_source; for 'mempalace' a drawer_id prefix. Omit to target every record of that kind." },
                                        "dry_run": { "type": "boolean", "default": true, "description": "When true (the default), report what would be removed and change nothing." },
                                        "limit": { "type": "integer", "default": 500, "minimum": UNDO_IMPORT_LIMIT_MIN, "maximum": UNDO_IMPORT_LIMIT_MAX, "description": "Maximum memories to remove per call. Work is resumable." }
                                    },
                                    "required": ["import_kind"]
                                }
                            },
                            {
                                "name": "remind_me_recalibrate_candidates",
                                "description": "Fetch memories whose importance classification may be stale: they look important (high base_weight, or a durable memory_type like decision/fact) yet have gone untouched for 90+ days and have never received feedback. Read-only — review each and apply changes with remind_me_reclassify (a type change) or remind_me_feedback (a pure importance nudge). Most candidates will look fine; this only narrows an unbounded set to a reviewable batch.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "limit": { "type": "integer", "default": 20, "minimum": RECALIBRATE_LIMIT_MIN, "maximum": RECALIBRATE_LIMIT_MAX, "description": "Number of importance-review candidates to return" }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_normalize_apply",
                                "description": "Write distilled normalizations back as new memories, each linked to the raw import it came from via a normalized_from metadata pointer. The raw memory is left untouched.",
                                "inputSchema": normalize_apply_input_schema()
                            },
                            {
                                "name": "remind_me_entity_traverse",
                                "description": "Multi-hop traversal of the typed entity-relation graph from a starting entity. Unlike remind_me_entity (one entity's direct facts) this follows entity_relations edges in both directions, so it answers questions that chain relations rather than co-mention.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string", "description": "Entity name or alias to start from (case/whitespace-insensitive)", "minLength": 1, "maxLength": 200 },
                                        "hops": { "type": "integer", "default": 1, "minimum": 1, "maximum": 3, "description": "Maximum traversal depth. 1 = direct relations only." },
                                        "relation": { "type": "string", "maxLength": 200, "description": "Optional: only follow edges whose relation label matches exactly" },
                                        "cap": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100, "description": "Max number of relation edges to return" }
                                    },
                                    "required": ["name"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_write",
                                "description": "Write or update a markdown wiki page.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "title": { "type": "string", "minLength": 1, "maxLength": 200, "description": "The title's slug is the page's identity — keep titles stable so [[wikilinks]] resolve" },
                                        "content": { "type": "string", "minLength": 1, "maxLength": 100000, "description": "Full markdown body, REPLACING any existing content. Open with a one-sentence summary; it becomes the index entry. A leading '# Title' is added if absent." },
                                        "log_note": { "type": "string", "maxLength": 500, "description": "Optional note recorded in log.md alongside the change" }
                                    },
                                    "required": ["title", "content"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_read",
                                "description": "Read a wiki page by slug.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "slug": { "type": "string" }
                                    },
                                    "required": ["slug"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_import",
                                "description": "Import a directory of Markdown files into the wiki. Each file's slug/title/topic come from its YAML front matter, falling back to its first '# ' heading then its filename. Idempotent - pages upsert on slug. Pairs with `dbs export-wiki --out-dir` from daily-backup-system.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "dir": { "type": "string" },
                                        "recursive": { "type": "boolean", "default": true }
                                    },
                                    "required": ["dir"]
                                }
                            },
                            {
                                "name": "remind_me_reclassify",
                                "description": "Apply memory_type classifications. Updates each memory's decay rate to match its new type.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "classifications": {
                                            "type": "array",
                                            "minItems": 1,
                                            "maxItems": 100,
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "memory_id": { "type": "string" },
                                                    "memory_type": { "type": "string" }
                                                },
                                                "required": ["memory_id", "memory_type"]
                                            }
                                        }
                                    },
                                    "required": ["classifications"]
                                }
                            },
                            {
                                "name": "remind_me_reclassify_batch",
                                "description": "Fetch a batch of still-unclassified memories, with content snippets, to classify and feed back through remind_me_reclassify.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "batch_size": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_feedback",
                                "description": "Mark a memory helpful or unhelpful. Without `query` this is a global judgement that adjusts the memory's weight; with `query` it is recorded as feedback for similar future searches only.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "memory_id": { "type": "string" },
                                        "signal": { "type": "string", "enum": ["helpful", "unhelpful"] },
                                        "query": { "type": "string", "maxLength": 500 }
                                    },
                                    "required": ["memory_id", "signal"]
                                }
                            },
                            {
                                "name": "remind_me_backup",
                                "description": "Create a WAL-safe online backup of the memory database, beside the database file. Older backups beyond the retention count are pruned.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {}
                                }
                            },
                            {
                                "name": "remind_me_annotate",
                                "description": "Apply subject/predicate/object triples and entity mentions to existing memories, in batches of up to 100.",
                                "inputSchema": annotate_input_schema()
                            },
                            {
                                "name": "remind_me_vitality_report",
                                "description": "Vault health report: active/dormant counts, average vitality, distribution buckets, and a breakdown by category.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default, as the reference) returns the structured report; markdown renders it for reading." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_wiki_load",
                                "description": "Load the whole wiki into context as one markdown document, newest-revised first up to a token budget. Overflow is listed by title so it can be read individually.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "token_budget": { "type": "integer", "default": 0, "minimum": 0, "maximum": 200000, "description": "0 means unlimited" },
                                        "include_index": { "type": "boolean", "default": true }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_wiki_compile",
                                "description": "Drive wiki synthesis over raw memories. With mark_integrated=false (the default) returns a brief of pending sources and never advances the watermark, so it is safe to call repeatedly. Call again with mark_integrated=true after writing the pages.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "json", "description": "json (default) returns the structured record; markdown returns a human-readable summary." },
                                        "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 },
                                        "mark_integrated": { "type": "boolean", "default": false }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_wiki_search",
                                "description": "Full-text search wiki page titles and content, ranked by BM25.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": { "type": "string", "minLength": 1, "maxLength": 500 },
                                        "limit": { "type": "integer", "default": 10, "minimum": 1, "maximum": 50 }
                                    },
                                    "required": ["query"]
                                }
                            },
                            {
                                "name": "remind_me_wiki_list",
                                "description": "List every wiki page, most recently updated first.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown", "description": "markdown (default, as the reference) renders the wiki index; json returns count and pages." },}
                                }
                            },
                            {
                                "name": "remind_me_wiki_delete",
                                "description": "Delete a wiki page by title or slug.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "title": {
                                            "type": "string",
                                            "description": "Page title or slug",
                                            "minLength": 1,
                                            "maxLength": 200
                                        }
                                    },
                                    "required": ["title"]
                                }
                            },
                            {
                                "name": "remind_me_consolidate",
                                "description": "Find clusters of near-duplicate memories by embedding similarity and optionally merge them into one canonical representative. dry_run (default true) reports clusters — canonical, members, and each member's similarity to the canonical — without changing anything. To actually merge, review the report, write a short summary per cluster you want consolidated, then call again with dry_run=false and summaries={canonical_id: summary}; a cluster with no matching summary is skipped, not merged with a raw concatenation.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "similarity_threshold": { "type": "number", "default": 0.85, "minimum": CONSOLIDATE_SIMILARITY_MIN, "maximum": CONSOLIDATE_SIMILARITY_MAX, "description": "Minimum cosine similarity to cluster memories together. Higher = stricter." },
                                        "dry_run": { "type": "boolean", "default": true, "description": "If true, report clusters without modifying data. Set false to auto-merge." },
                                        "category": { "type": "string", "description": "Limit consolidation to this category" },
                                        "limit": { "type": "integer", "default": 500, "minimum": CONSOLIDATE_LIMIT_MIN, "maximum": CONSOLIDATE_LIMIT_MAX, "description": "Maximum memories to consider (prevents runaway on large vaults)" },
                                        "summaries": { "type": "object", "additionalProperties": { "type": "string" }, "description": "{canonical_id: summary}, one entry per cluster (from a prior dry_run=true call) you want consolidated. Required to actually merge a cluster when dry_run=false." }
                                    }
                                }
                            },
                            {
                                "name": "remind_me_stats",
                                "description": "Get database stats and memory counts.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "response_format": { "type": "string", "enum": ["markdown", "json"], "default": "markdown" }
                                    }
                                }
                            }
                        ]
                    }
                });
                // Pruned once, here, after the whole surface is declared —
                // rather than by guarding each entry, which would put the
                // tier decision in 62 places and let the next tool forget it.
                let profile = remind_me_core::tool_profiles::configured_profile();
                if let Some(tools) = listed["result"]["tools"].as_array_mut() {
                    tools.retain(|tool| {
                        tool.get("name")
                            .and_then(Value::as_str)
                            .map(|name| remind_me_core::tool_profiles::tool_allowed(&profile, name))
                            .unwrap_or(true)
                    });
                }
                Some(listed)
            }
            "resources/list" => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "resources": [
                            {
                                "uri": "memory://stats",
                                "name": "Memory Engine Statistics",
                                "mimeType": "application/json"
                            }
                        ]
                    }
                }))
            }
            "resources/read" => {
                let req_id = id.unwrap_or(json!(1));
                let conn = self.db.conn();
                match stats::collect(&conn) {
                    Ok(s) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "contents": [
                                {
                                    "uri": "memory://stats",
                                    "mimeType": "application/json",
                                    "text": serde_json::to_string_pretty(&s).unwrap()
                                }
                            ]
                        }
                    })),
                    Err(e) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32603, "message": format!("Stats error: {}", e) }
                    })),
                }
            }
            "prompts/list" => {
                let req_id = id.unwrap_or(json!(1));
                let mut listed = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "prompts": [
                            {
                                "name": "recall_context",
                                "description": "Recall long-term memory facts relevant to the ongoing conversation topic.",
                                "arguments": [
                                    { "name": "topic", "description": "Topic or entity keyword to search", "required": true }
                                ]
                            }
                        ]
                    }
                });
                // A no-op today — this crate offers no maintenance prompts yet
                // — but wired now so one added later is hidden with the tier
                // it drives. A prompt sequencing invisible tools walks the
                // model into calls that will be refused.
                let profile = remind_me_core::tool_profiles::configured_profile();
                if let Some(prompts) = listed["result"]["prompts"].as_array_mut() {
                    prompts.retain(|prompt| {
                        prompt
                            .get("name")
                            .and_then(Value::as_str)
                            .map(|name| {
                                remind_me_core::tool_profiles::prompt_allowed(&profile, name)
                            })
                            .unwrap_or(true)
                    });
                }
                Some(listed)
            }
            "tools/call" => {
                let req_id = id.unwrap_or(json!(1));
                let params = req.get("params")?;
                let tool_name = params.get("name")?.as_str()?;
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // Read before `args` is moved into any tool's input model.
                let format = requested_format(&args);
                let conn = self.db.conn();
                // Hidden means gone, not merely undocumented: a model that
                // guessed the name would otherwise still reach it, and a
                // caller who trimmed their surface would never know it was
                // porous.
                let profile = remind_me_core::tool_profiles::configured_profile();
                if !remind_me_core::tool_profiles::tool_allowed(&profile, tool_name) {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "isError": true,
                            "content": [{
                                "type": "text",
                                "text": format!(
                                    "Tool '{}' is not available under the '{}' tool profile. \
                                     Set {}=full to expose the whole surface.",
                                    tool_name,
                                    profile,
                                    remind_me_core::tool_profiles::TOOL_PROFILE_ENV
                                )
                            }]
                        }
                    }));
                }
                let mut span = remind_me_core::telemetry::maybe_span(&format!("tool.{tool_name}"));
                // Started unconditionally: `record_tool_call` decides for
                // itself whether metrics are on, the same way `maybe_span`
                // above does, so this dispatch never grows an `if enabled`.
                let started = std::time::Instant::now();
                // Reference-counted for the life of this dispatch. The guard
                // drops at the end of this arm -- including on an early return
                // or a panic -- so a stuck call cannot leave the watchdog armed
                // forever, and an overlapping call keeps it armed after this
                // one finishes.
                let _watchdog = remind_me_core::watchdog::arm(tool_name);

                let result = match tool_name {
                    "remind_me_add" => {
                        let input: Result<MemoryAddInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(add_input) => match queries::add_memory(&conn, add_input) {
                                Ok(mem) => {
                                    json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&mem).unwrap(),
    ResponseFormat::Markdown => render::memory_stored(&mem),
} }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Database error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_get" => {
                        // "memory_id" is the canonical parameter, matching every
                        // other single-memory tool. "id" is a deprecated alias
                        // kept so existing callers don't break; it's only
                        // consulted when "memory_id" is absent.
                        let id = args
                            .get("memory_id")
                            .and_then(|v| v.as_str())
                            .or_else(|| args.get("id").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        match queries::get_memory_by_id(&conn, id) {
                            Ok(Some(mem)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&mem).unwrap() }] })
                            }
                            Ok(None) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": "Memory not found" }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_list" => {
                        let input: Result<MemoryListInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(list_input) => match queries::list_memories(&conn, &list_input) {
                                Ok(page) => {
                                    // Same as search above: the field was
                                    // parsed with the reference's Markdown
                                    // default (models.py:365) and discarded.
                                    let text = match list_input.response_format {
                                        ResponseFormat::Json => {
                                            serde_json::to_string_pretty(&page).unwrap()
                                        }
                                        ResponseFormat::Markdown => {
                                            remind_me_core::reminders::render_memory_page_markdown(
                                                &page.memories,
                                                page.total,
                                            )
                                        }
                                    };
                                    json!({ "content": [{ "type": "text", "text": text }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("List error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid list input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_update" => {
                        let input: Result<MemoryUpdateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(update_input) => {
                                match queries::update_memory(&conn, &update_input) {
                                    Ok(UpdateOutcome::Updated(mem)) => {
                                        json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&mem).unwrap(),
    ResponseFormat::Markdown => render::memory_updated(&mem),
} }] })
                                    }
                                    Ok(UpdateOutcome::NotFound) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", update_input.memory_id) }] })
                                    }
                                    Ok(UpdateOutcome::NoFields) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": "Nothing to update — no fields provided" }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Update error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid update input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_delete" => {
                        let memory_id =
                            args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
                        match queries::delete_memory(&conn, memory_id) {
                            Ok(true) => {
                                json!({ "content": [{ "type": "text", "text": format!("Memory `{}` deleted", memory_id) }] })
                            }
                            Ok(false) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", memory_id) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Delete error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_search" => {
                        let input: Result<MemorySearchInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(search_input) => {
                                match queries::search_with_expansions(&conn, &search_input) {
                                    Ok(res) => {
                                        // `MemorySearchInput` already carries
                                        // `response_format`, already defaulting
                                        // to Markdown like the reference's
                                        // `MemorySearchInput` (models.py:199).
                                        // The value was parsed and then thrown
                                        // away here, so a markdown request got
                                        // a successful JSON response (#224).
                                        let text = match search_input.response_format {
                                            ResponseFormat::Json => {
                                                serde_json::to_string_pretty(&res).unwrap()
                                            }
                                            ResponseFormat::Markdown => {
                                                render::search_response(&res)
                                            }
                                        };
                                        json!({ "content": [{ "type": "text", "text": text }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Search error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid search input: {}", e) }] })
                            }
                        }
                    }
                    // Read-only, matching the reference. A miss reports
                    // `found: false` rather than creating the entity -- see
                    // `EntityLookupInput`'s docs for why that distinction is
                    // load-bearing rather than cosmetic.
                    "remind_me_entity" => {
                        let input: Result<EntityLookupInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(lookup) => {
                                let limit = lookup
                                    .limit
                                    .clamp(ENTITY_LOOKUP_LIMIT_MIN, ENTITY_LOOKUP_LIMIT_MAX);
                                match entity::entity_profile(&conn, &lookup.name, limit) {
                                    Ok(Some(profile)) => {
                                        // `found` alongside the profile's own
                                        // fields, not wrapping them -- the
                                        // reference spreads the payload
                                        // (`{"found": True, **profile}`), and a
                                        // caller written against one shape
                                        // must not have to unwrap the other.
                                        let mut payload =
                                            serde_json::to_value(&profile).unwrap_or(json!({}));
                                        if let Some(object) = payload.as_object_mut() {
                                            object.insert("found".into(), json!(true));
                                        }
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap() }] })
                                    }
                                    // Not `isError`: an unknown name is a valid
                                    // answer to a lookup, and the reference
                                    // returns a normal payload for it too.
                                    Ok(None) => {
                                        let payload = json!({
                                            "found": false,
                                            "query": lookup.name,
                                            "message": format!("No entity found matching {:?}.", lookup.name),
                                        });
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Entity error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid entity input: {}", e) }] })
                            }
                        }
                    }
                    // Target-only: the reference has no upsert tool. This is
                    // where `remind_me_entity`'s old write behaviour lives, so
                    // the capability is kept rather than dropped -- it is just
                    // no longer reachable by a call that meant to read.
                    "remind_me_entity_upsert" => {
                        let input: Result<EntityInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(ent_input) => match entity::upsert_entity(&conn, &ent_input) {
                                Ok(ent) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&ent).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Entity error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid entity input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_extract_batch" => {
                        let input: ExtractBatchInput = serde_json::from_value(args)
                            .unwrap_or(ExtractBatchInput { batch_size: 20 });
                        match queries::unannotated_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Extract batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_import_chat" => {
                        let input: Result<ChatImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => match importer::import_chat(&conn, &import_input) {
                                Ok(outcome) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Import error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_import_directory" => {
                        let input: Result<BulkImportDirInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => {
                                match importer::import_directory(&conn, &import_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Directory import error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid directory import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_watch_status" => {
                        // The running loop first (#203). Falling straight to
                        // `from_env()` would report on a Watcher constructed
                        // microseconds ago that has never scanned, so every
                        // counter would read zero while a real loop was busy.
                        let mut report = match watcher::live_status() {
                            Some(live) => live,
                            None => match watcher::Watcher::from_env() {
                                Some(w) => w.status(),
                                None => watcher::disabled_status(),
                            },
                        };
                        // Added by the tool rather than the watcher, matching
                        // the reference: the count is a property of the wiki,
                        // not of the folder scan, and the watcher has no
                        // connection to ask.
                        report.pending_wiki_compile =
                            remind_me_core::wiki_fs::pending_compile_count(&conn).unwrap_or(0);
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                    }
                    "remind_me_check_update" => {
                        let status = updater::check_for_update();
                        json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&status).unwrap(),
    ResponseFormat::Markdown => render::update_status(&status),
} }] })
                    }
                    "remind_me_self_update" => {
                        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                        let result = updater::perform_update(force);
                        if result.success {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                        } else {
                            json!({ "isError": true, "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                        }
                    }
                    "remind_me_revoke_clients" => {
                        let client_id = args.get("client_id").and_then(Value::as_str).unwrap_or("");
                        let store = remind_me_core::remote::OAuthStateStore::new(
                            remind_me_core::remote::oauth_state_file_path(),
                        );
                        let body = if client_id.is_empty() {
                            json!({
                                "clients": store.list_clients(),
                                "state_file": store.path().to_string_lossy(),
                                "hint": "Pass client_id to revoke a client and all of its tokens.",
                            })
                        } else {
                            match store.revoke_client(client_id) {
                                Ok(Some(summary)) => {
                                    let mut body =
                                        serde_json::to_value(summary).unwrap_or(json!({}));
                                    body["status"] = json!("revoked");
                                    body
                                }
                                Ok(None) => json!({
                                    "status": "error",
                                    "error": format!("Unknown client_id: {}", client_id),
                                }),
                                // Distinct from "unknown client": the client
                                // exists and is STILL AUTHORIZED. Reporting
                                // it as revoked would be the worst possible
                                // answer here (issue #160).
                                Err(e) => json!({
                                    "status": "error",
                                    "error": format!(
                                        "Could not persist revocation for {}: {}. \
                                         The client remains authorized.",
                                        client_id, e
                                    ),
                                }),
                            }
                        };
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                    }
                    "remind_me_reindex" => match vectors::reindex(&conn) {
                        Ok(result) => {
                            json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&result).unwrap(),
    ResponseFormat::Markdown => render::reindex_result(&result),
} }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reindex error: {}", e) }] })
                        }
                    },
                    "remind_me_import_dbs" => {
                        let input: Result<DbsImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => match dbs_import::pull_dbs(&conn, &import_input) {
                                Ok(result) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("dbs import error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid dbs import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_import_mempalace" => {
                        let input: Result<MempalaceImportInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(import_input) => {
                                match mempalace_import::pull_mempalace(&conn, &import_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("MemPalace import error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid MemPalace import input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_webhook_status" => {
                        let report = self.webhook.status();
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                    }
                    "remind_me_list_connectors" => {
                        let body = json!({
                            "connectors": importer::connectors(),
                            "file_import_kinds": ["chat", "document"],
                        });
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                    }
                    "remind_me_server_status" => match status::server_status(&conn) {
                        Ok(report) => {
                            // The webhook's/sync peer's state lives on these
                            // structs, not on the connection, so they are
                            // merged in here rather than gathered by
                            // `server_status`. The sync worker's state does
                            // not live on `self` at all -- unlike the webhook
                            // and sync peer, it is not something this struct
                            // owns (see the `McpServer` field doc), so its
                            // status is read the same way
                            // `remind_me_watch_status` already reads the
                            // watcher's: a process-global "whichever one is
                            // actually running" lookup, not an instance field.
                            let mut report = serde_json::to_value(&report).unwrap_or(json!({}));
                            report["webhook"] =
                                serde_json::to_value(self.webhook.status()).unwrap_or(json!({}));
                            report["sync_peer"] =
                                serde_json::to_value(self.sync_peer.status()).unwrap_or(json!({}));
                            report["sync"] = serde_json::to_value(
                                remind_me_core::sync::sync_live_status(&conn).unwrap_or_else(
                                    remind_me_core::sync::sync_worker_disabled_status,
                                ),
                            )
                            .unwrap_or(json!({}));
                            // The remote MCP connector (FT-05, #85) has no
                            // running state to merge in the way the webhook/
                            // sync do -- remind_me_remote::run() only exists
                            // when the CLI process opts in, and this crate
                            // stays synchronous, so there is no live server
                            // handle here to ask. remind_me_core::remote::remote_status
                            // reports config/token state the same way the
                            // reference's get_remote_status() does, purely
                            // from env vars and the token file.
                            report["remote"] =
                                serde_json::to_value(remind_me_core::remote::remote_status())
                                    .unwrap_or(json!({}));
                            // Embeddings (#90): `status::server_status` above
                            // already reports config-only state without a
                            // network call (its own stated contract);
                            // `embedding_status` adds the live "and
                            // reachable" probe (cached -- see
                            // `available_embedder`), matching the
                            // sync/webhook/remote overrides' shape: replace,
                            // don't merge.
                            report["embeddings"] =
                                serde_json::to_value(remind_me_core::embedder::embedding_status())
                                    .unwrap_or(json!({}));
                            // Dashboard (#90): genuinely cross-process --
                            // `rusty-remind-me api` is a separate OS process
                            // from this one, so the only shared state is the
                            // PID file it writes on start. An in-memory
                            // database has nowhere to put one, which is not
                            // an error: it just means there is no dashboard
                            // to find.
                            report["dashboard"] = match remind_me_core::pid::pid_file_path(&conn)
                            {
                                Ok(path) => serde_json::to_value(
                                    remind_me_core::pid::dashboard_status(&path),
                                )
                                .unwrap_or(json!({})),
                                Err(_) => serde_json::to_value(status::SubsystemStatus::NotImplemented {
                                    reason: "in-memory database has no on-disk location for a dashboard PID file".to_string(),
                                })
                                .unwrap_or(json!({})),
                            };
                            {
                                let text = match format {
                                    ResponseFormat::Json => {
                                        serde_json::to_string_pretty(&report).unwrap()
                                    }
                                    ResponseFormat::Markdown => render::server_status(&report),
                                };
                                json!({ "content": [{ "type": "text", "text": text }] })
                            }
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Server status error: {}", e) }] })
                        }
                    },
                    "remind_me_export_memories" => {
                        let input: ExportInput = serde_json::from_value(args).unwrap_or_default();
                        match export::export_memories(&conn, &input) {
                            Ok(result) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Export error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_decompose" => {
                        let input: Result<DecomposeInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(decompose_input) => {
                                if decompose_input.facts.len() < DECOMPOSE_FACTS_MIN
                                    || decompose_input.facts.len() > DECOMPOSE_FACTS_MAX
                                {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("facts must hold {}..={} entries", DECOMPOSE_FACTS_MIN, DECOMPOSE_FACTS_MAX) }] })
                                } else {
                                    match capture::decompose(&conn, &decompose_input) {
                                        Ok(Some(result)) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                        }
                                        Ok(None) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("No memory found with capture_id {:?}.", decompose_input.capture_id) }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Decompose error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid decompose input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_decompose_batch" => {
                        let input: DecomposeBatchInput = serde_json::from_value(args)
                            .unwrap_or(DecomposeBatchInput { batch_size: 20 });
                        match capture::undecomposed_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Decompose batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_auto_capture" => {
                        let input: Result<AutoCaptureInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(capture_input) => {
                                match capture::auto_capture(&conn, &capture_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&result).unwrap(),
    ResponseFormat::Markdown => render::capture_result(&result),
} }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Auto capture error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid auto capture input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_get_capture" => {
                        let capture_id = args
                            .get("capture_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match capture::get_capture(&conn, capture_id) {
                            Ok(Some(found)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&found).unwrap() }] })
                            }
                            Ok(None) => {
                                json!({ "content": [{ "type": "text", "text": format!("No capture found with id {:?}.", capture_id) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Get capture error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_archive_prune" => {
                        // Defaults to a dry run: this is the one tool here
                        // that deletes data the caller cannot get back, so
                        // the destructive reading must be the explicit one.
                        let dry_run = args
                            .get("dry_run")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        match remind_me_core::archive::prune(&conn, dry_run) {
                            Ok(report) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Archive prune error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_promotion_candidates" => {
                        let rung: std::result::Result<remind_me_core::Rung, _> =
                            serde_json::from_value(
                                args.get("rung").cloned().unwrap_or(serde_json::Value::Null),
                            );
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(PROMOTION_LIMIT_DEFAULT as u64)
                            as usize;
                        match rung {
                            Ok(rung) => {
                                match remind_me_core::promotion::promotion_candidates(
                                    &conn, rung, limit,
                                ) {
                                    Ok(found) => {
                                        // The whole backlog rides along, not
                                        // just this rung's: a caller working
                                        // one rung has no other way to notice
                                        // the one below it filling up.
                                        let backlog = remind_me_core::promotion::backlog(&conn)
                                            .unwrap_or_default();
                                        // Uncapped, unlike `backlog` above (which probes
                                        // only up to BACKLOG_PROBE per rung so it stays
                                        // cheap): this is the real count behind *this*
                                        // rung's page, so it keeps moving past the probe
                                        // ceiling instead of reading as stuck (#283).
                                        let total_candidates =
                                            remind_me_core::promotion::count_candidates(
                                                &conn, rung,
                                            )
                                            .unwrap_or_default();
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&json!({
                                            "rung": rung,
                                            "count": found.len(),
                                            "candidates": found,
                                            "total_candidates": total_candidates,
                                            "backlog": backlog,
                                            "backlog_summary": backlog.summary(),
                                            // Not `nudge_interval().is_some()`: that only
                                            // proves an interval is *configured*, which
                                            // `start_nudge_for` already requires before it
                                            // registers a loop, so a crashed nudge thread and
                                            // an unconfigured one used to read identically
                                            // here -- both `true`, only one of them lying
                                            // (#270).
                                            "nudge_enabled": remind_me_core::promotion::nudge_running(),
                                        })).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Promotion candidates error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid rung: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_promote" => {
                        let parsed: std::result::Result<remind_me_core::PromoteInput, _> =
                            serde_json::from_value(args.clone());
                        match parsed {
                            Ok(input) => match remind_me_core::promotion::promote(&conn, &input) {
                                Ok(result) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Promotion error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid promotion input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_stale_candidates" => {
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(STALE_CANDIDATES_LIMIT_DEFAULT as u64)
                            as usize;
                        match remind_me_core::code_refs::stale_candidates(&conn, limit) {
                            Ok(found) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&json!({
                                    "count": found.candidates.len(),
                                    "candidates": found.candidates,
                                    "total_candidates": found.total_candidates,
                                })).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Stale candidates error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_persona" => {
                        let include_demoted = args
                            .get("include_demoted")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        match remind_me_core::promotion::persona(&conn) {
                            Ok(statements) => {
                                let demoted = if include_demoted {
                                    remind_me_core::promotion::demoted(&conn).unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                let mut payload = json!({
                                    "count": statements.len(),
                                    "persona": statements,
                                });
                                if include_demoted {
                                    payload["demoted"] = json!(demoted);
                                }
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Persona error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_provenance" => {
                        let memory_id =
                            args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
                        match remind_me_core::promotion::provenance(&conn, memory_id) {
                            Ok(found) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&found).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Provenance error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_skeleton_write" => {
                        let parsed: std::result::Result<remind_me_core::SkeletonWriteInput, _> =
                            serde_json::from_value(args.clone());
                        match parsed {
                            Ok(input) => {
                                match remind_me_core::skeleton::write_skeleton(&conn, &input) {
                                    Ok(written) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&written).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Skeleton write error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid skeleton input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_skeleton_read" => {
                        let capture_id = args
                            .get("capture_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // An empty `node` is treated as absent rather than as a
                        // node named "": a client that sends the field
                        // unconditionally should get the diagram, not a
                        // not-found for a node nobody could have written.
                        let node = args
                            .get("node")
                            .and_then(|v| v.as_str())
                            .filter(|n| !n.trim().is_empty());
                        match node {
                            Some(node) => {
                                match remind_me_core::skeleton::node_slice(&conn, capture_id, node)
                                {
                                    Ok(Some(slice)) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&slice).unwrap() }] })
                                    }
                                    Ok(None) => {
                                        json!({ "content": [{ "type": "text", "text": format!("No node {:?} in the skeleton for capture {:?}.", node, capture_id) }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Skeleton read error: {}", e) }] })
                                    }
                                }
                            }
                            None => {
                                match remind_me_core::skeleton::read_skeleton(&conn, capture_id) {
                                    Ok(Some(skeleton)) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&skeleton).unwrap() }] })
                                    }
                                    Ok(None) => {
                                        json!({ "content": [{ "type": "text", "text": format!("No skeleton for capture {:?}. Write one with remind_me_skeleton_write.", capture_id) }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Skeleton read error: {}", e) }] })
                                    }
                                }
                            }
                        }
                    }
                    "remind_me_source" => {
                        let memory_id =
                            args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("");
                        let include_sensitive = args
                            .get("include_sensitive")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        match remind_me_core::archive::source_for(
                            &conn,
                            memory_id,
                            include_sensitive,
                        ) {
                            Ok(Some(found)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&found).unwrap() }] })
                            }
                            // One message for every "nothing to show" case, on
                            // purpose. Distinguishing "retention was off" from
                            // "this memory is sensitive" would let a caller
                            // probe the sensitive flag of memories it is being
                            // refused, which is the one thing the refusal is
                            // for.
                            Ok(None) => {
                                json!({ "content": [{ "type": "text", "text": format!(
                                    "No archived source for memory {:?}. Raw retention is only recorded for file imports made while REMIND_ME_ARCHIVE_DIR was set.",
                                    memory_id
                                ) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Source lookup error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_normalize_batch" => {
                        let input: NormalizeBatchInput = serde_json::from_value(args)
                            .unwrap_or(NormalizeBatchInput { batch_size: 20 });
                        match normalize::unnormalized_batch(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Normalize batch error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_sync_reconcile" => {
                        match remind_me_core::sync::reconcile_hub(&conn) {
                            Ok(report) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reconcile error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_sync_reconcile_peer" => {
                        match serde_json::from_value::<ReconcilePeerInput>(args) {
                            Ok(input) => {
                                match remind_me_core::sync::reconcile_peer(&conn, &input.node_id) {
                                    Ok(report) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reconcile peer error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid reconcile_peer input: {}. node_id is required.", e) }] })
                            }
                        }
                    }
                    "remind_me_api_key" => {
                        use remind_me_core::api_keys;
                        let action = args
                            .get("action")
                            .and_then(Value::as_str)
                            .unwrap_or("list")
                            .to_string();
                        let name = args
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        match action.as_str() {
                            "list" => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&api_keys::list_keys()).unwrap() }] })
                            }
                            "issue" => {
                                let scope = args
                                    .get("scope")
                                    .and_then(Value::as_str)
                                    .unwrap_or(api_keys::SCOPE_READ);
                                match api_keys::create_key(&name, scope) {
                                    // The only time the plaintext exists in
                                    // readable form anywhere. Said plainly,
                                    // because a caller who does not copy it
                                    // now cannot recover it later.
                                    Ok(plaintext) => {
                                        json!({ "content": [{ "type": "text", "text": format!(
                                        "Issued API key '{}' (scope={}).\n\n{}\n\nThis is the only time this key is shown — only its hash is stored. Copy it now; if lost, revoke it and issue another.",
                                        name, scope, plaintext
                                    ) }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Could not issue key: {}", e) }] })
                                    }
                                }
                            }
                            "revoke" => match api_keys::revoke_key(&name) {
                                Ok(true) => {
                                    json!({ "content": [{ "type": "text", "text": format!("Revoked API key '{}'.", name) }] })
                                }
                                // Distinct from success: reporting a revoke
                                // that did nothing would leave the caller
                                // believing a key they cannot see is gone.
                                Ok(false) => {
                                    json!({ "content": [{ "type": "text", "text": format!("No API key named '{}'. Nothing was revoked.", name) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Could not revoke key: {}", e) }] })
                                }
                            },
                            other => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Unknown action '{}'; expected issue, list, or revoke.", other) }] })
                            }
                        }
                    }
                    "remind_me_reminders_ics_url" => {
                        // The path is always returned; the full URL only when
                        // there is an HTTP surface to serve it from. A stdio-only
                        // connection has none, and inventing a base would hand
                        // back a URL that silently never resolves.
                        let token = remind_me_core::ics::resolve_ics_token();
                        let feed_path = remind_me_core::ics::feed_path(&token);
                        let status = remind_me_core::pid::pid_file_path(&conn)
                            .ok()
                            .map(|path| remind_me_core::pid::dashboard_status(&path));
                        let text = match status.filter(|s| s.running).and_then(|s| s.url) {
                            Some(base) => format!("{}{}", base.trim_end_matches('/'), feed_path),
                            None => format!(
                                "No HTTP surface is currently active to serve the reminders \
                                 calendar feed. Start the dashboard (`rusty-remind-me api`) and \
                                 call this tool again for the full URL. Feed path once running: {}",
                                feed_path
                            ),
                        };
                        json!({ "content": [{ "type": "text", "text": text }] })
                    }
                    "remind_me_set_reminder" => {
                        match serde_json::from_value::<SetReminderInput>(args) {
                            Ok(input) => match remind_me_core::reminders::set_reminder(
                                &conn,
                                &input.memory_id,
                                input.remind_at.as_deref(),
                            ) {
                                Ok(outcome) => {
                                    json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&outcome).unwrap(),
    ResponseFormat::Markdown => render::set_reminder_outcome(&outcome),
} }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Set reminder error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid set_reminder input: {}. memory_id is required.", e) }] })
                            }
                        }
                    }
                    "remind_me_list_reminders" => {
                        let input: ListRemindersInput =
                            serde_json::from_value(args).unwrap_or_default();
                        let limit = input.limit.clamp(REMINDER_LIMIT_MIN, REMINDER_LIMIT_MAX);
                        match remind_me_core::reminders::list_reminders(&conn, input.when, limit) {
                            Ok(memories) => {
                                let text = match input.response_format {
                                    ResponseFormat::Json => json!({
                                        "count": memories.len(),
                                        "memories": memories,
                                    })
                                    .to_string(),
                                    ResponseFormat::Markdown => {
                                        remind_me_core::reminders::render_memories_markdown(
                                            &memories,
                                        )
                                    }
                                };
                                json!({ "content": [{ "type": "text", "text": text }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("List reminders error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_sync_status" => match remind_me_core::sync::sync_status(&conn) {
                        Ok(status) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&status).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Sync status error: {}", e) }] })
                        }
                    },
                    "remind_me_sync_repair" => {
                        let input: SyncRepairInput =
                            serde_json::from_value(args).unwrap_or(SyncRepairInput {
                                remote_id: "hub".to_string(),
                            });
                        match remind_me_core::sync::sync_repair(&conn, &input.remote_id) {
                            Ok(true) => {
                                json!({ "content": [{ "type": "text", "text": format!("Reset pull cursors for '{}'. The next sync will re-pull its history.", input.remote_id) }] })
                            }
                            // Distinct from success: a remote that was never
                            // contacted has nothing to repair, and reporting
                            // success would send the caller waiting for a
                            // re-pull that is not coming.
                            Ok(false) => {
                                json!({ "content": [{ "type": "text", "text": format!("No sync_log row for '{}' — nothing to repair.", input.remote_id) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Sync repair error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_digest" => {
                        let mut input: DigestInput =
                            serde_json::from_value(args).unwrap_or(DigestInput {
                                since_days: digest::DEFAULT_SINCE_DAYS,
                                response_format: Default::default(),
                            });
                        input.since_days = input
                            .since_days
                            .clamp(digest::DIGEST_SINCE_DAYS_MIN, digest::DIGEST_SINCE_DAYS_MAX);
                        match digest::build_digest(&conn, input.since_days) {
                            Ok(data) => {
                                let text = match input.response_format {
                                    ResponseFormat::Json => {
                                        serde_json::to_string_pretty(&data).unwrap()
                                    }
                                    ResponseFormat::Markdown => digest::render_markdown(&data),
                                };
                                json!({ "content": [{ "type": "text", "text": text }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Digest error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_contradiction_candidates" => {
                        let mut input: ContradictionCandidatesInput = serde_json::from_value(args)
                            .unwrap_or(ContradictionCandidatesInput {
                                limit: 20,
                                after_a: None,
                                after_b: None,
                            });
                        input.limit = input
                            .limit
                            .clamp(CONTRADICTION_LIMIT_MIN, CONTRADICTION_LIMIT_MAX);
                        match input.cursor() {
                            // A half cursor is refused, not ignored: paging
                            // from the start while the caller believes it is
                            // resuming is the same invisible-no-progress
                            // failure the cursor exists to fix.
                            Err(message) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": message }] })
                            }
                            Ok(cursor) => {
                                match contradictions::candidates(&conn, input.limit, cursor) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Contradiction candidates error: {}", e) }] })
                                    }
                                }
                            }
                        }
                    }
                    "remind_me_history" => match serde_json::from_value::<HistoryInput>(args) {
                        Ok(mut input) => {
                            input.limit = input.limit.clamp(HISTORY_LIMIT_MIN, HISTORY_LIMIT_MAX);
                            if !history::memory_is_live(&conn, &input.memory_id).unwrap_or(false) {
                                json!({ "content": [{ "type": "text", "text": format!("Memory '{}' not found.", input.memory_id) }] })
                            } else {
                                match history::history(&conn, &input.memory_id, input.limit) {
                                    Ok(revisions) => {
                                        let text = match input.response_format {
                                            // The reference's JSON branch is an
                                            // envelope, not the bare array this
                                            // used to emit -- `count` saves a
                                            // caller re-deriving what the
                                            // producer already knew.
                                            ResponseFormat::Json => {
                                                serde_json::to_string_pretty(&json!({
                                                    "memory_id": input.memory_id,
                                                    "count": revisions.len(),
                                                    "revisions": revisions,
                                                }))
                                                .unwrap()
                                            }
                                            ResponseFormat::Markdown => {
                                                history::render_revisions_markdown(
                                                    &input.memory_id,
                                                    &revisions,
                                                )
                                            }
                                        };
                                        json!({ "content": [{ "type": "text", "text": text }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("History error: {}", e) }] })
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid history input: {}", e) }] })
                        }
                    },
                    "remind_me_revert" => match serde_json::from_value::<RevertInput>(args) {
                        Ok(input) => match history::revert(
                            &conn,
                            &input.memory_id,
                            input.revision_id,
                            input.reason.as_deref(),
                        ) {
                            Ok(outcome) => {
                                json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&outcome).unwrap(),
    ResponseFormat::Markdown => render::revert_outcome(&outcome),
} }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Revert error: {}", e) }] })
                            }
                        },
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid revert input: {}", e) }] })
                        }
                    },
                    "remind_me_save_search" => {
                        match serde_json::from_value::<SaveSearchInput>(args) {
                            Ok(input) => match saved_searches::save_search(&conn, &input) {
                                Ok(saved) => {
                                    json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&saved).unwrap(),
    ResponseFormat::Markdown => render::saved_search(&saved),
} }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Save search error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid save_search input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_list_saved_searches" => {
                        match saved_searches::list_saved_searches(&conn) {
                            Ok(searches) => {
                                json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&searches).unwrap(),
    ResponseFormat::Markdown => render::saved_search_list(&searches),
} }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("List saved searches error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_run_saved_search" => {
                        match serde_json::from_value::<SavedSearchNameInput>(args) {
                            Ok(input) => match saved_searches::get_saved_search(&conn, &input.name)
                            {
                                Ok(Some(saved)) => {
                                    match saved_searches::run_saved_search(&conn, &saved) {
                                        Ok(results) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&results).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Run saved search error: {}", e) }] })
                                        }
                                    }
                                }
                                // A missing name is a caller mistake with an
                                // obvious remedy, not a server error — same
                                // posture the reference takes.
                                Ok(None) => {
                                    json!({ "content": [{ "type": "text", "text": format!("Saved search '{}' not found.", input.name) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Run saved search error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid run_saved_search input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_delete_saved_search" => {
                        match serde_json::from_value::<SavedSearchNameInput>(args) {
                            Ok(input) => {
                                match saved_searches::delete_saved_search(&conn, &input.name) {
                                    Ok(true) => {
                                        json!({ "content": [{ "type": "text", "text": format!("Saved search '{}' deleted.", input.name) }] })
                                    }
                                    Ok(false) => {
                                        json!({ "content": [{ "type": "text", "text": format!("Saved search '{}' not found.", input.name) }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Delete saved search error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid delete_saved_search input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_undo_import" => {
                        // Unlike the batch readers, a malformed input is
                        // rejected rather than defaulted: there is no safe
                        // default for *which* import to destroy, and guessing
                        // one is how you delete the wrong thing.
                        match serde_json::from_value::<UndoImportInput>(args) {
                            Ok(mut input) => {
                                input.limit = input
                                    .limit
                                    .clamp(UNDO_IMPORT_LIMIT_MIN, UNDO_IMPORT_LIMIT_MAX);
                                match undo_import::undo_import(&conn, &input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Undo import error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid undo_import input: {}. import_kind must be one of chat, dbs, mempalace.", e) }] })
                            }
                        }
                    }
                    "remind_me_recalibrate_candidates" => {
                        // Same clamp-don't-reject posture as the other batch
                        // readers here: an out-of-range limit is a caller
                        // mistake with an obvious right answer, and refusing
                        // costs a round trip to learn a bound the schema
                        // already advertises.
                        let mut input: RecalibrateCandidatesInput = serde_json::from_value(args)
                            .unwrap_or(RecalibrateCandidatesInput { limit: 20 });
                        input.limit = input
                            .limit
                            .clamp(RECALIBRATE_LIMIT_MIN, RECALIBRATE_LIMIT_MAX);
                        match recalibrate::candidates(&conn, &input) {
                            Ok(batch) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&batch).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Recalibrate candidates error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_normalize_apply" => {
                        let input: Result<NormalizeApplyInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(apply_input) => {
                                if apply_input.normalizations.len() < NORMALIZE_APPLY_MIN
                                    || apply_input.normalizations.len() > NORMALIZE_APPLY_MAX
                                {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("normalizations must hold {}..={} entries", NORMALIZE_APPLY_MIN, NORMALIZE_APPLY_MAX) }] })
                                } else {
                                    match normalize::apply_normalizations(&conn, &apply_input) {
                                        Ok(result) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Normalize apply error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid normalize apply input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_entity_traverse" => {
                        let input: Result<EntityTraverseInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(traverse_input) => {
                                match entity::traverse_from_name(&conn, &traverse_input) {
                                    Ok(result) => {
                                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                    }
                                    Err(e) => {
                                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Entity traverse error: {}", e) }] })
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid entity traverse input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_write" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let log_note = args.get("log_note").and_then(|v| v.as_str());
                        // The slug is derived from the title now that files are
                        // canonical: a caller-supplied slug disagreeing with the
                        // title would name a file the index could not find. A
                        // `slug` argument is still tolerated as the title when
                        // none is given, so an older caller is not broken.
                        let title = if title.is_empty() { slug } else { title };
                        match self.wiki.write_page(&conn, title, content, log_note) {
                            Ok(Ok(outcome)) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                            }
                            Ok(Err(_)) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("'{}' is a reserved system page and cannot be written directly", title) }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki write error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_read" => {
                        let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                        match self.wiki.read_page(&conn, slug) {
                            Ok(Some(page)) => {
                                json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&page).unwrap(),
    ResponseFormat::Markdown => render::wiki_page(&page),
} }] })
                            }
                            Ok(None) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": "Wiki page not found" }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki read error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_import" => {
                        let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("");
                        let recursive = args
                            .get("recursive")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        match wiki_import::import_wiki_dir(
                            &conn,
                            std::path::Path::new(dir),
                            recursive,
                        ) {
                            Ok(report) => {
                                let imported: Vec<_> = report.imported.iter().map(|p| json!({
                                    "slug": p.slug, "title": p.title, "topic": p.topic, "path": p.path
                                })).collect();
                                let skipped: Vec<_> = report
                                    .skipped
                                    .iter()
                                    .map(|(path, reason)| {
                                        json!({
                                            "path": path, "reason": reason
                                        })
                                    })
                                    .collect();
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&json!({
                                    "imported_count": imported.len(),
                                    "skipped_count": skipped.len(),
                                    "imported": imported,
                                    "skipped": skipped
                                })).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki import error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_reclassify" => {
                        let input: Result<ReclassifyInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(rc) => {
                                let count = rc.classifications.len();
                                if !(RECLASSIFY_BATCH_MIN..=RECLASSIFY_BATCH_MAX).contains(&count) {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("`classifications` must hold {}..={} items, got {}", RECLASSIFY_BATCH_MIN, RECLASSIFY_BATCH_MAX, count) }] })
                                } else {
                                    match queries::reclassify_memories(&conn, &rc) {
                                        Ok(outcome) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reclassify error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid reclassify input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_reclassify_batch" => {
                        let input: Result<ReclassifyBatchInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(batch) => match queries::unclassified_batch(&conn, &batch) {
                                Ok(result) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Reclassify batch error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid batch input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_feedback" => {
                        let input: Result<FeedbackInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(fb) => match vitality::record_feedback(
                                &conn,
                                &fb.memory_id,
                                fb.signal,
                                fb.query.as_deref(),
                            ) {
                                Ok(Some(v)) => {
                                    let body = json!({
                                        "memory_id": fb.memory_id,
                                        "signal": fb.signal,
                                        "vitality": v
                                    });
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                                }
                                Ok(None) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Memory `{}` not found", fb.memory_id) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Feedback error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid feedback input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_backup" => match backup::create_backup(&conn, "manual") {
                        Ok(outcome) => {
                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Backup error: {}", e) }] })
                        }
                    },
                    "remind_me_annotate" => {
                        let input: Result<AnnotateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(annotate_input) => {
                                let count = annotate_input.annotations.len();
                                if !(ANNOTATE_BATCH_MIN..=ANNOTATE_BATCH_MAX).contains(&count) {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("`annotations` must hold {}..={} items, got {}", ANNOTATE_BATCH_MIN, ANNOTATE_BATCH_MAX, count) }] })
                                } else {
                                    match queries::annotate_memories(&conn, &annotate_input) {
                                        Ok(outcome) => {
                                            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&outcome).unwrap() }] })
                                        }
                                        Err(e) => {
                                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Annotate error: {}", e) }] })
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid annotate input: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_vitality_report" => match vitality::build_vitality_report(&conn) {
                        Ok(report) => {
                            // The one reference model that defaults to JSON
                            // (`VitalityReportInput`, models.py:1653), so the
                            // default here was already right -- by absence
                            // rather than intent. Markdown was simply
                            // unreachable; now it is opt-in, matching.
                            let text = match format_or(&args, ResponseFormat::Json) {
                                ResponseFormat::Json => {
                                    serde_json::to_string_pretty(&report).unwrap()
                                }
                                ResponseFormat::Markdown => render::vitality_report(&report),
                            };
                            json!({ "content": [{ "type": "text", "text": text }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Vitality report error: {}", e) }] })
                        }
                    },
                    "remind_me_wiki_load" => {
                        let token_budget = args
                            .get("token_budget")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(0);
                        let include_index = args
                            .get("include_index")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        match self.wiki.load(&conn, token_budget, include_index) {
                            Ok(loaded) if loaded.pages_included == 0 => {
                                json!({ "content": [{ "type": "text", "text": "_The wiki is empty._ Synthesise pages from raw memories with `remind_me_wiki_compile`." }] })
                            }
                            Ok(loaded) => {
                                json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&loaded).unwrap() }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki load error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_compile" => {
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(20)
                            .clamp(1, 100);
                        let mark_integrated = args
                            .get("mark_integrated")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        match self.wiki.compile(&conn, limit, mark_integrated) {
                            Ok(outcome) => {
                                json!({ "content": [{ "type": "text", "text": match format {
    ResponseFormat::Json => serde_json::to_string_pretty(&outcome).unwrap(),
    ResponseFormat::Markdown => render::wiki_compile(&outcome),
} }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki compile error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_wiki_search" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(wiki::WIKI_SEARCH_LIMIT_DEFAULT);
                        if query.is_empty() {
                            json!({ "isError": true, "content": [{ "type": "text", "text": "`query` is required" }] })
                        } else {
                            match self.wiki.search_pages(&conn, query, limit) {
                                Ok(hits) => {
                                    let body = json!({ "count": hits.len(), "results": hits });
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&body).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki search error: {}", e) }] })
                                }
                            }
                        }
                    }
                    "remind_me_wiki_list" => match self.wiki.list_pages(&conn) {
                        Ok(pages) => {
                            // The reference's `WikiListInput` defaults to
                            // MARKDOWN (models.py:1547); this had no field at
                            // all and always returned JSON. Read from raw args
                            // rather than adding an input model, because that is
                            // all this arm has ever taken.
                            let text = match format_or(&args, ResponseFormat::Markdown) {
                                ResponseFormat::Json => {
                                    let body = json!({ "count": pages.len(), "pages": pages });
                                    serde_json::to_string_pretty(&body).unwrap()
                                }
                                ResponseFormat::Markdown => render::wiki_page_list(&pages),
                            };
                            json!({ "content": [{ "type": "text", "text": text }] })
                        }
                        Err(e) => {
                            json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki list error: {}", e) }] })
                        }
                    },
                    "remind_me_wiki_delete" => {
                        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        if title.is_empty() {
                            json!({ "isError": true, "content": [{ "type": "text", "text": "`title` is required" }] })
                        } else {
                            match self.wiki.delete_page(&conn, title) {
                                Ok(WikiDeleteOutcome::Deleted) => {
                                    json!({ "content": [{ "type": "text", "text": format!("Wiki page '{}' deleted", title) }] })
                                }
                                Ok(WikiDeleteOutcome::NotFound) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki page '{}' not found", title) }] })
                                }
                                Ok(WikiDeleteOutcome::Reserved) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("'{}' is a reserved system page and cannot be deleted", title) }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Wiki delete error: {}", e) }] })
                                }
                            }
                        }
                    }
                    "remind_me_stats" => {
                        // `unwrap_or_default` rather than erroring on a bad
                        // payload: the only field is optional, so an absent or
                        // malformed body still has one sensible reading.
                        let input: MemoryStatsInput =
                            serde_json::from_value(args).unwrap_or_default();
                        match stats::collect(&conn) {
                            Ok(s) => {
                                let text = match input.response_format {
                                    ResponseFormat::Json => {
                                        serde_json::to_string_pretty(&s).unwrap()
                                    }
                                    ResponseFormat::Markdown => stats::render_markdown(&s),
                                };
                                json!({ "content": [{ "type": "text", "text": text }] })
                            }
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Stats error: {}", e) }] })
                            }
                        }
                    }
                    "remind_me_consolidate" => {
                        let input: Result<ConsolidateInput, _> = serde_json::from_value(args);
                        match input {
                            Ok(consolidate_input) => match consolidate(&conn, &consolidate_input) {
                                Ok(report) => {
                                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&report).unwrap() }] })
                                }
                                Err(e) => {
                                    json!({ "isError": true, "content": [{ "type": "text", "text": format!("Consolidate error: {}", e) }] })
                                }
                            },
                            Err(e) => {
                                json!({ "isError": true, "content": [{ "type": "text", "text": format!("Invalid consolidate input: {}", e) }] })
                            }
                        }
                    }
                    _ => {
                        json!({ "isError": true, "content": [{ "type": "text", "text": format!("Unknown tool: {}", tool_name) }] })
                    }
                };
                if result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    span.mark_error();
                }
                // Recorded for failed calls too. A tool that errors still
                // consumed time and still says something about how the
                // server is being used; counting only successes would make a
                // wholly broken tool look like an unused one.
                remind_me_core::metrics::record_tool_call(
                    tool_name,
                    started.elapsed().as_secs_f64(),
                );

                let mut result = result;
                // Surfaces once, on whatever tool call happens to be first
                // after startup, then clears — matching the reference's own
                // one-shot startup notice, attached centrally here rather
                // than duplicated per handler since this dispatch has a
                // single point every tool call already passes through.
                if let Some(notice) = updater::pop_update_notice() {
                    if let Some(content) = result.get_mut("content").and_then(|c| c.as_array_mut())
                    {
                        content.push(json!({ "type": "text", "text": notice }));
                    }
                }

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": result
                }))
            }
            _ => {
                let req_id = id.unwrap_or(json!(1));
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": "Method not found"
                    }
                }))
            }
        }
    }

    pub fn run_stdio_loop(&self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let handle = stdin.lock();

        for line in handle.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(resp) = dispatch_line_catching_panics(&line, |l| self.handle_request(l)) {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{}", resp_str)?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

/// Runs `handler` against one line of input, converting a panic inside it
/// into the same shape of JSON-RPC error every other failure path in
/// `handle_request` returns, instead of letting it unwind out of the stdio
/// loop and take every other in-flight and future request down with it. A
/// panic inside a single tool call (an unwrap on unexpected input, an
/// out-of-bounds index) used to do exactly that, since `handle_request` ran
/// on the loop's own unguarded call stack. `db.conn()`'s lock is a
/// `parking_lot::Mutex`, which does not poison on an unwind through a held
/// guard, so a caught panic mid-call leaves the database perfectly usable
/// for the next request.
///
/// Factored out from [`McpServer::run_stdio_loop`] so a test can supply a
/// deliberately-panicking `handler` and observe the wrapping behavior
/// directly, without needing a real bug in `handle_request` to trigger one.
fn dispatch_line_catching_panics(
    line: &str,
    handler: impl FnOnce(&str) -> Option<Value>,
) -> Option<Value> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(line))) {
        Ok(resp) => resp,
        Err(_) => Some(panicked_response(line)),
    }
}

/// Add `"resultType": "complete"` to a reply's `result` object when it
/// carries none.
///
/// The `2026-07-28` MCP revision made `resultType` a required member of
/// every result — `"complete"` for an ordinary reply, `"input_required"`
/// for a multi-round-trip interim one — and a client at that revision
/// rejects a result without it; only a server negotiated at an *earlier*
/// revision gets the absent-means-complete reading. This server answers
/// `initialize` with the client's own requested version, so a client that
/// asks for `2026-07-28` is told it got it, and every subsequent reply
/// then failed that client's validation: no tool could be called at all.
///
/// Stamped unconditionally rather than per negotiated version: the
/// handler keeps no per-connection state (the streamable-HTTP transport
/// in `remind_me_remote` is stateless by design), and a pre-`2026-07-28`
/// client ignores a member it does not know, as every revision's
/// forward-compatibility rule requires. A reply that already names its
/// `resultType` — a future `input_required` — is left alone, as is a
/// JSON-RPC `error` reply, which has no `result` to stamp.
fn stamp_result_type(response: &mut Value) {
    let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    result
        .entry("resultType")
        .or_insert_with(|| json!("complete"));
}

/// The JSON-RPC error reply for a request whose handler panicked.
///
/// Echoes back whatever `id` the request carried (re-parsed independently
/// of `handle_request`, since that call is the one that just panicked and
/// its own parse of `id` never returned) so the client can still match this
/// error to the call it made, the same as every other error reply in
/// `handle_request` does.
fn panicked_response(request_json: &str) -> Value {
    let req_id = serde_json::from_str::<Value>(request_json)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(json!(1));
    json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {
            "code": -32603,
            "message": "internal error: request handler panicked"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Held by every test that reads or sets `REMIND_ME_EMBEDDING_BACKEND`
    /// (a process-global env var): `remind_me_server_status`'s `embeddings`
    /// override (`#90`) now reflects it, so a test asserting the unset
    /// default must not race one that configures a backend. Same convention
    /// as `remind_me_core`'s own `sync_test.rs`/`status_test.rs` `ENV_LOCK`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_panicking_handler_produces_an_internal_error_response_instead_of_unwinding() {
        let resp = dispatch_line_catching_panics(
            r#"{"jsonrpc":"2.0","id":42,"method":"whatever"}"#,
            |_line| panic!("simulated handler panic"),
        )
        .unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
        assert_eq!(resp["error"]["code"], -32603);
    }

    #[test]
    fn a_panicking_handler_on_unparseable_input_gets_the_fallback_id() {
        let resp =
            dispatch_line_catching_panics("not even json", |_line| panic!("simulated panic"))
                .unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["error"]["code"], -32603);
    }

    #[test]
    fn a_non_panicking_handler_passes_its_response_through_unchanged() {
        let resp = dispatch_line_catching_panics(r#"{"id":7}"#, |_line| {
            Some(json!({ "jsonrpc": "2.0", "id": 7, "result": {} }))
        });
        assert_eq!(
            resp,
            Some(json!({ "jsonrpc": "2.0", "id": 7, "result": {} }))
        );
    }

    #[test]
    fn a_non_panicking_handler_that_returns_none_passes_none_through() {
        // Matches notifications, which `handle_request` also answers with
        // `None` -- the stdio loop must not synthesize a response for them.
        let resp =
            dispatch_line_catching_panics(r#"{"method":"notifications/initialized"}"#, |_line| {
                None
            });
        assert_eq!(resp, None);
    }

    #[test]
    fn a_caught_panic_while_holding_the_db_lock_still_leaves_it_usable() {
        // What makes catching the panic here safe rather than merely quiet:
        // unwinding through a held `db.conn()` guard runs its destructor the
        // same as any other scope exit, and `parking_lot::Mutex` (unlike
        // `std::sync::Mutex`) never poisons on that unwind -- so the lock is
        // free again by the time `catch_unwind` returns. A synthetic panic
        // stands in for whatever future bug in `handle_request` would
        // trigger this for real, since no request panics through real
        // dispatch today.
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let resp = dispatch_line_catching_panics(r#"{"id":1}"#, |_line| {
            let _guard = server.db.conn();
            panic!("simulated panic while a connection guard is live")
        });
        assert_eq!(resp.unwrap()["error"]["code"], -32603);

        let ping = json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" });
        let resp = server.handle_request(&ping.to_string()).unwrap();
        assert_eq!(resp["result"], json!({ "resultType": "complete" }));
    }

    #[test]
    fn test_mcp_initialize_dynamic_version() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28"
            }
        });
        let resp = server.handle_request(&req.to_string()).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2026-07-28");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], true);
    }

    /// A `2026-07-28` client requires `resultType` on every result, and a
    /// server that echoes that version in `initialize` has promised it.
    /// Before this stamp every tool call was rejected by such a client as
    /// malformed — the symptom that motivated it. Every method that answers
    /// with a `result` carries the member; a JSON-RPC `error` does not.
    #[test]
    fn every_result_carries_result_type_complete() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let with_result = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2026-07-28" } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/read",
                    "params": { "uri": "memory://stats" } }),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "prompts/list" }),
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                    "params": { "name": "remind_me_add",
                                "arguments": { "content": "resultType is required" } } }),
            // A tool execution error is still a result, not a JSON-RPC error.
            json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
                    "params": { "name": "remind_me_get",
                                "arguments": { "memory_id": "mem_missing" } } }),
        ];
        for req in with_result {
            let resp = server.handle_request(&req.to_string()).unwrap();
            assert_eq!(
                resp["result"]["resultType"], "complete",
                "{} must answer with resultType: complete — got {resp}",
                req["method"]
            );
        }
        let error_req = json!({ "jsonrpc": "2.0", "id": 9, "method": "no/such/method" });
        let resp = server.handle_request(&error_req.to_string()).unwrap();
        assert!(resp.get("result").is_none(), "{resp}");
        assert_eq!(resp["error"]["code"], -32601);

        // A result that already names its type is left alone.
        let mut interim = json!({ "jsonrpc": "2.0", "id": 10,
                                  "result": { "resultType": "input_required" } });
        stamp_result_type(&mut interim);
        assert_eq!(interim["result"]["resultType"], "input_required");
    }

    fn call(server: &McpServer, name: &str, args: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        server.handle_request(&req.to_string()).unwrap()["result"].clone()
    }

    fn text_of(result: &Value) -> String {
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn test_crud_tools_are_registered() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        for expected in ["remind_me_list", "remind_me_update", "remind_me_delete"] {
            assert!(names.contains(&expected), "{} not in tools/list", expected);
        }
    }

    #[test]
    fn test_crud_tools_round_trip_over_jsonrpc() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(
            &server,
            "remind_me_add",
            json!({ "content": "a fact worth keeping" }),
        );
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // `json` is explicit now: `remind_me_list` defaults to Markdown,
        // matching the reference's `MemoryListInput` (#224). This test is
        // about the round trip, not the rendering, so it asks for the
        // structured form rather than parsing prose.
        let listed = call(
            &server,
            "remind_me_list",
            json!({ "response_format": "json" }),
        );
        let page: Value = serde_json::from_str(&text_of(&listed)).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["memories"][0]["id"], id);

        let updated = call(
            &server,
            "remind_me_update",
            json!({ "memory_id": id, "content": "a revised fact" }),
        );
        assert!(
            updated.get("isError").is_none(),
            "update failed: {:?}",
            updated
        );
        assert!(text_of(&updated).contains("a revised fact"));

        let deleted = call(&server, "remind_me_delete", json!({ "memory_id": id }));
        assert!(
            deleted.get("isError").is_none(),
            "delete failed: {:?}",
            deleted
        );

        let after = call(
            &server,
            "remind_me_list",
            json!({ "response_format": "json" }),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&after)).unwrap()["total"],
            0
        );
    }

    #[test]
    fn test_crud_tools_surface_errors() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let missing = call(
            &server,
            "remind_me_delete",
            json!({ "memory_id": "mem_nope" }),
        );
        assert_eq!(missing["isError"], true);

        let no_fields = call(
            &server,
            "remind_me_update",
            json!({ "memory_id": "mem_nope" }),
        );
        assert_eq!(no_fields["isError"], true);

        // `limit` is typed usize; a negative value must be an input error rather
        // than a silent clamp to zero.
        let bad_limit = call(&server, "remind_me_list", json!({ "limit": -3 }));
        assert_eq!(bad_limit["isError"], true);
    }

    #[test]
    fn test_feedback_tool_both_modes() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(&server, "remind_me_add", json!({ "content": "a fact" }));
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Global: no query, so the weight moves and the response reports it.
        let global = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": id, "signal": "helpful" }),
        );
        assert!(global.get("isError").is_none(), "failed: {:?}", global);
        let body: Value = serde_json::from_str(&text_of(&global)).unwrap();
        assert_eq!(body["signal"], "helpful");
        assert!(body["vitality"].as_f64().unwrap() > 1.0);

        // Contextual: a query is supplied, so vitality is reported unchanged.
        let contextual = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": id, "signal": "unhelpful", "query": "some question" }),
        );
        let after: Value = serde_json::from_str(&text_of(&contextual)).unwrap();
        assert_eq!(after["vitality"], body["vitality"]);
    }

    #[test]
    fn test_feedback_rejects_a_bad_signal_and_unknown_memory() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let bad = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": "mem_x", "signal": "maybe" }),
        );
        assert_eq!(bad["isError"], true, "signal is a closed set of two values");

        let missing = call(
            &server,
            "remind_me_feedback",
            json!({ "memory_id": "mem_nope", "signal": "helpful" }),
        );
        assert_eq!(missing["isError"], true);
    }

    #[test]
    fn test_backup_tool_is_registered_and_takes_no_parameters() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_backup")
            .expect("remind_me_backup not in tools/list");

        assert!(
            tool["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .is_empty(),
            "backup takes no caller-supplied destination"
        );
    }

    #[test]
    fn test_backup_of_an_in_memory_database_reports_why() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let result = call(&server, "remind_me_backup", json!({}));
        assert_eq!(result["isError"], true);
        assert!(
            text_of(&result).contains("in memory"),
            "expected an explanation, got: {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_remind_me_get_schema_advertises_memory_id_as_required() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap().clone();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "remind_me_get")
            .expect("remind_me_get must still be declared");

        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        assert!(
            props.contains_key("memory_id"),
            "memory_id must be advertised, matching every other single-memory tool"
        );
        assert!(
            props.contains_key("id"),
            "id stays advertised as a deprecated alias so existing schema-reading clients still see it"
        );
        let required: Vec<&str> = tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            required,
            vec!["memory_id"],
            "memory_id is the canonical required param, not id"
        );
    }

    #[test]
    fn test_remind_me_get_accepts_both_memory_id_and_the_deprecated_id_alias() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(&server, "remind_me_add", json!({ "content": "a fact" }));
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Canonical: memory_id.
        let via_memory_id = call(&server, "remind_me_get", json!({ "memory_id": id }));
        assert!(via_memory_id.get("isError").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&via_memory_id)).unwrap()["id"],
            id
        );

        // Deprecated alias: id, for backward compatibility (#283).
        let via_id = call(&server, "remind_me_get", json!({ "id": id }));
        assert!(via_id.get("isError").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&via_id)).unwrap()["id"],
            id
        );

        // memory_id wins when both are somehow present.
        let both = call(
            &server,
            "remind_me_get",
            json!({ "memory_id": id, "id": "mem_nope" }),
        );
        assert!(both.get("isError").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&both)).unwrap()["id"],
            id
        );
    }

    #[test]
    fn test_annotate_tool_round_trip_and_partial_failure() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let added = call(&server, "remind_me_add", json!({ "content": "a fact" }));
        let id = serde_json::from_str::<Value>(&text_of(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let annotated = call(
            &server,
            "remind_me_annotate",
            json!({ "annotations": [
                { "memory_id": id, "predicate": "uses", "entities": [{ "name": "SQLite" }] },
                { "memory_id": "mem_nope", "predicate": "uses" }
            ]}),
        );
        assert!(annotated.get("isError").is_none());

        let body: Value = serde_json::from_str(&text_of(&annotated)).unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["errors"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["entities_linked"], 1);

        let fetched = call(&server, "remind_me_get", json!({ "id": id }));
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&fetched)).unwrap()["predicate"],
            "uses"
        );
    }

    #[test]
    fn test_annotate_rejects_an_out_of_range_batch() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let empty = call(&server, "remind_me_annotate", json!({ "annotations": [] }));
        assert_eq!(empty["isError"], true);

        let oversized: Vec<Value> = (0..ANNOTATE_BATCH_MAX + 1)
            .map(|_| json!({ "memory_id": "mem_x" }))
            .collect();
        let too_many = call(
            &server,
            "remind_me_annotate",
            json!({ "annotations": oversized }),
        );
        assert_eq!(too_many["isError"], true);
    }

    #[test]
    fn test_vitality_report_tool() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_vitality_report")
            .expect("remind_me_vitality_report not in tools/list");
        assert_eq!(
            tool["inputSchema"]["properties"]["response_format"]["default"], "json",
            "the reference defaults this tool to JSON, unlike most others"
        );
        // The schema literal used to define `response_format` twice inside
        // the same object -- legal Rust (duplicate keys in a `json!` macro
        // just overwrite each other), but the second copy carried no
        // `description` and a reversed `enum` order. Asserting both here
        // pins the single surviving definition and would catch either kind
        // of regression: a dropped description, or the pre-fix duplicate
        // silently winning again.
        assert_eq!(
            tool["inputSchema"]["properties"]["response_format"]["enum"],
            json!(["markdown", "json"])
        );
        assert!(tool["inputSchema"]["properties"]["response_format"]["description"].is_string());

        call(&server, "remind_me_add", json!({ "content": "a memory" }));
        let report = call(&server, "remind_me_vitality_report", json!({}));
        assert!(
            report.get("isError").is_none(),
            "report failed: {:?}",
            report
        );

        let body: Value = serde_json::from_str(&text_of(&report)).unwrap();
        assert_eq!(body["total_memories"], 1);
        assert_eq!(body["vault_health_score"], "100%");
        assert!(body["vitality_buckets"].is_object());
    }

    #[test]
    fn test_consolidate_tool_is_registered_and_defaults_to_a_safe_dry_run() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_consolidate")
            .expect("remind_me_consolidate not in tools/list");
        let schema = &tool["inputSchema"]["properties"];
        assert_eq!(schema["dry_run"]["default"], true);
        assert_eq!(schema["similarity_threshold"]["default"], 0.85);
        assert_eq!(schema["similarity_threshold"]["minimum"], 0.5);
        assert_eq!(schema["similarity_threshold"]["maximum"], 1.0);
        assert_eq!(schema["limit"]["default"], 500);
        assert_eq!(schema["limit"]["minimum"], 10);
        assert_eq!(schema["limit"]["maximum"], 5000);

        // No embedder is configured in this test harness, so nothing has a
        // chunk-0 vector -- the round trip still exercises real dispatch and
        // must report cleanly rather than erroring.
        call(&server, "remind_me_add", json!({ "content": "a memory" }));
        let result = call(&server, "remind_me_consolidate", json!({}));
        assert!(result.get("isError").is_none(), "call failed: {:?}", result);

        let body: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(body["clusters_found"], 0);
        assert_eq!(body["message"], "No eligible memories found");
    }

    /// A server whose wiki lives in its own scratch directory.
    ///
    /// `McpServer::new` reads the configured wiki root, which is a real shared
    /// directory — a test using it would write into the machine user's actual
    /// wiki.
    fn wiki_server(name: &str) -> (McpServer, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("rrm_mcp_wiki_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let db = Database::open_in_memory().unwrap();
        (
            McpServer::with_wiki(db, remind_me_core::wiki_fs::Wiki::new(&root)),
            root,
        )
    }

    #[test]
    fn test_wiki_tools_are_registered() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();

        for expected in ["remind_me_wiki_list", "remind_me_wiki_delete"] {
            assert!(names.contains(&expected), "{} not in tools/list", expected);
        }
    }

    #[test]
    fn test_wiki_tools_round_trip_over_jsonrpc() {
        let (server, root) = wiki_server("roundtrip");

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "title": "VLAN Setup", "content": "body" }),
        );

        // Same as the crud round trip above: `remind_me_wiki_list` now
        // defaults to Markdown like the reference's `WikiListInput`.
        let listed = call(
            &server,
            "remind_me_wiki_list",
            json!({ "response_format": "json" }),
        );
        let page: Value = serde_json::from_str(&text_of(&listed)).unwrap();
        assert_eq!(page["count"], 1);
        assert_eq!(page["pages"][0]["slug"], "vlan-setup");

        // Address it by human title rather than slug.
        let deleted = call(
            &server,
            "remind_me_wiki_delete",
            json!({ "title": "VLAN Setup" }),
        );
        assert!(
            deleted.get("isError").is_none(),
            "delete failed: {:?}",
            deleted
        );

        let after = call(
            &server,
            "remind_me_wiki_list",
            json!({ "response_format": "json" }),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&text_of(&after)).unwrap()["count"],
            0
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_wiki_search_tool() {
        let (server, root) = wiki_server("search");

        let req = json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        assert!(
            resp["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "remind_me_wiki_search"),
            "remind_me_wiki_search not in tools/list"
        );

        call(
            &server,
            "remind_me_wiki_write",
            json!({ "title": "VLAN Setup", "content": "trunking notes" }),
        );

        let hits = call(
            &server,
            "remind_me_wiki_search",
            json!({ "query": "trunking" }),
        );
        assert!(hits.get("isError").is_none(), "search failed: {:?}", hits);
        let body: Value = serde_json::from_str(&text_of(&hits)).unwrap();
        assert_eq!(body["count"], 1);
        // The slug is derived from the title, and the summary from the first
        // body line — neither is caller-supplied any more, because a file-backed
        // page has to be self-describing.
        assert_eq!(body["results"][0]["slug"], "vlan-setup");
        assert_eq!(body["results"][0]["summary"], "trunking notes");

        let missing = call(&server, "remind_me_wiki_search", json!({}));
        assert_eq!(missing["isError"], true);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_memory_search_survives_punctuation() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(
            &server,
            "remind_me_add",
            json!({ "content": "the plan is to ship" }),
        );

        // Unsanitised, this is an FTS5 syntax error rather than a search.
        let found = call(
            &server,
            "remind_me_search",
            json!({ "query": "what's the plan, exactly?" }),
        );
        assert!(
            found.get("isError").is_none(),
            "search errored: {:?}",
            found
        );
        assert!(text_of(&found).contains("the plan is to ship"));
    }

    #[test]
    fn test_wiki_delete_surfaces_errors() {
        let (server, root) = wiki_server("deleteerrors");

        let missing = call(&server, "remind_me_wiki_delete", json!({ "title": "nope" }));
        assert_eq!(missing["isError"], true);

        let blank = call(&server, "remind_me_wiki_delete", json!({ "title": "" }));
        assert_eq!(blank["isError"], true);

        // Writing a reserved page is refused too, not just deleting one.
        let written = call(
            &server,
            "remind_me_wiki_write",
            json!({ "title": "Index", "content": "body" }),
        );
        assert_eq!(written["isError"], true);
        assert!(text_of(&written).contains("reserved"));

        let reserved = call(
            &server,
            "remind_me_wiki_delete",
            json!({ "title": "index" }),
        );
        assert_eq!(reserved["isError"], true);
        assert!(text_of(&reserved).contains("reserved"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_import_dbs_is_registered_and_refuses_a_path_outside_the_roots() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 12, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_import_dbs")
            .expect("remind_me_import_dbs not in tools/list");
        assert_eq!(tool["inputSchema"]["required"][0], "db_path");
        assert_eq!(tool["inputSchema"]["properties"]["limit"]["maximum"], 2000);

        let result = call(
            &server,
            "remind_me_import_dbs",
            json!({ "db_path": "/etc/hosts" }),
        );

        assert_eq!(result["isError"], true);
        // Refused for containment, not for existence — the message must not
        // reveal whether a path outside the roots is there.
        assert!(
            text_of(&result).contains("not in allowed import roots"),
            "got {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_import_mempalace_is_registered_and_has_no_path_field() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 13, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_import_mempalace")
            .expect("remind_me_import_mempalace not in tools/list");
        // Unlike remind_me_import_dbs, the store location is operator
        // configuration (REMIND_ME_MEMPALACE_PATH), not a per-call argument —
        // there is nothing required at all.
        assert!(tool["inputSchema"]["required"]
            .as_array()
            .is_none_or(|r| r.is_empty()));
        assert_eq!(tool["inputSchema"]["properties"]["limit"]["maximum"], 2000);

        // No store configured in the test environment, so this must fail —
        // as "no store found", not silently succeed with zero results, which
        // would be indistinguishable from an empty palace.
        let result = call(&server, "remind_me_import_mempalace", json!({}));
        assert_eq!(result["isError"], true);
        assert!(
            text_of(&result).contains("No MemPalace store found"),
            "got {}",
            text_of(&result)
        );
    }

    #[test]
    fn test_webhook_status_is_registered_and_reports_disabled() {
        // No `REMIND_ME_WEBHOOK_SECRET` in the test environment, so nothing
        // binds a port — which is the state this asserts.
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"remind_me_webhook_status"),
            "remind_me_webhook_status not in tools/list"
        );

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_webhook_status",
            json!({}),
        )))
        .unwrap();
        assert_eq!(report["enabled"], false);
        assert_eq!(report["running"], false);
        // Disabled, not broken: no bind failure to report, and a hint saying
        // what would turn it on.
        assert!(report["start_error"].is_null());
        assert!(report["hint"].as_str().unwrap().contains("SECRET"));
    }

    #[test]
    fn test_check_update_and_self_update_are_registered() {
        // Not invoked here, unlike every other tool this file tests end to
        // end: both discover their repository from the process's current
        // working directory (docs/adr/0003-self-update-strategy.md), and
        // remind_me_self_update would really run `git pull`/`cargo build
        // --release -p rusty-remind-me -p remind_me_hub` against whatever repo contains this test
        // binary's cwd -- which, inside this workspace's own test suite, is
        // this very checkout. That behavior is exercised instead in
        // remind_me_core's updater.rs unit tests, against real but
        // disposable git repos under a temp directory, never this sandbox's
        // actual checkout.
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 15, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();

        let check = tools
            .iter()
            .find(|t| t["name"] == "remind_me_check_update")
            .expect("remind_me_check_update not in tools/list");
        // See the note on remind_me_reindex: `response_format` is the only
        // property, and it is presentational (#206).
        let props = check["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(
            props.keys().collect::<Vec<_>>(),
            vec!["response_format"],
            "remind_me_check_update should take no operational arguments"
        );

        let self_update = tools
            .iter()
            .find(|t| t["name"] == "remind_me_self_update")
            .expect("remind_me_self_update not in tools/list");
        assert_eq!(
            self_update["inputSchema"]["properties"]["force"]["default"],
            false
        );
    }

    #[test]
    fn test_revoke_clients_lists_by_default_and_revokes_one_client_by_id() {
        // No other test in this file touches REMIND_ME_REMOTE_OAUTH_STATE_FILE,
        // so (matching this file's existing convention, e.g. the reindex
        // test's EMBEDDING_BACKEND_ENV) this doesn't need a cross-test lock.
        let dir = std::env::temp_dir().join(format!(
            "rrm_mcp_revoke_clients_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("oauth.json");
        std::env::set_var(
            remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV,
            &state_file,
        );

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 16, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "remind_me_revoke_clients")
            .expect("remind_me_revoke_clients not in tools/list");
        assert_eq!(
            tool["inputSchema"]["properties"]["client_id"]["default"],
            ""
        );

        // Empty client_id is the *list* path, not "revoke every client" --
        // this is the exact semantics #86 called out as easy to get backwards.
        let empty_state: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(empty_state["clients"], json!([]));
        assert!(empty_state["hint"].as_str().unwrap().contains("client_id"));

        // Register a client with tokens directly against the same state
        // file a live remote server would read, the same cross-process
        // story the reference's tool relies on.
        let store = remind_me_core::remote::OAuthStateStore::new(&state_file);
        store
            .put_client(
                "client-1",
                json!({ "client_name": "claude.ai", "redirect_uris": ["https://claude.ai/cb"] }),
            )
            .expect("write");
        store
            .put_token(
                remind_me_core::remote::TokenKind::Access,
                "access-tok",
                json!({ "client_id": "client-1" }),
            )
            .expect("write");
        store
            .put_token(
                remind_me_core::remote::TokenKind::Refresh,
                "refresh-tok",
                json!({ "client_id": "client-1" }),
            )
            .expect("write");

        let listed: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(listed["clients"][0]["client_id"], "client-1");
        assert_eq!(listed["clients"][0]["access_tokens"], 1);
        assert_eq!(listed["clients"][0]["refresh_tokens"], 1);

        // A non-empty client_id revokes that one client and its tokens.
        let revoked: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({ "client_id": "client-1" }),
        )))
        .unwrap();
        assert_eq!(revoked["status"], "revoked");
        assert_eq!(revoked["access_tokens"], 1);
        assert_eq!(revoked["refresh_tokens"], 1);

        // The client is gone -- listing is empty again, not "still there".
        let after: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({}),
        )))
        .unwrap();
        assert_eq!(after["clients"], json!([]));

        // Revoking an unknown client_id is an error, never silently a no-op
        // success and never "revoked everything that happened to exist".
        let unknown: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_revoke_clients",
            json!({ "client_id": "no-such-client" }),
        )))
        .unwrap();
        assert_eq!(unknown["status"], "error");

        std::env::remove_var(remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reindex_is_registered_and_reports_degraded_without_an_embedder() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // No REMIND_ME_EMBEDDING_BACKEND in the test environment, so this
        // must report degraded rather than silently doing nothing.
        std::env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let req = json!({ "jsonrpc": "2.0", "id": 14, "method": "tools/list" });
        let resp = server.handle_request(&req.to_string()).unwrap();
        let tool = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_reindex")
            .expect("remind_me_reindex not in tools/list");
        // Exactly one property, and it is presentational. The original
        // assertion was `is_empty()`, meaning "this tool takes no arguments";
        // that intent survives #206, which added `response_format` and nothing
        // else. Naming the key keeps the guard as strict as it was rather than
        // loosening it to "some properties".
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(
            props.keys().collect::<Vec<_>>(),
            vec!["response_format"],
            "remind_me_reindex should take no operational arguments"
        );

        let report: Value =
            serde_json::from_str(&text_of(&call(&server, "remind_me_reindex", json!({})))).unwrap();

        assert_eq!(report["degraded"], true);
        assert_eq!(report["embedded"], 0);
    }

    /// The dispatch arm holds a watchdog guard for the life of the call, so a
    /// tool asking for status sees itself in flight. That self-count is the
    /// cheapest available proof that `tools/call` is actually armed — a guard
    /// that was never taken would report zero.
    #[test]
    fn test_a_tool_call_is_watched_while_it_runs() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_server_status",
            json!({}),
        )))
        .unwrap();

        assert_eq!(report["watchdog"]["enabled"], true);
        // `>= 1` rather than `== 1`: the suite runs tests in parallel against
        // the one process-wide watchdog, so a sibling test's call may legitimately
        // overlap this one.
        assert!(
            report["watchdog"]["calls_in_flight"].as_u64().unwrap_or(0) >= 1,
            "the in-flight status call should count itself, got: {}",
            report["watchdog"]
        );
    }

    /// The bug this split exists to remove: a lookup with a name that does
    /// not resolve used to *create* that entity. `remind_me` returns
    /// `found=false` and writes nothing, so a mistyped name silently forked
    /// the two vaults.
    #[test]
    fn test_entity_lookup_does_not_create_the_entity_it_failed_to_find() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let lookup = |name: &str| -> Value {
            serde_json::from_str(&text_of(&call(
                &server,
                "remind_me_entity",
                json!({ "name": name }),
            )))
            .unwrap()
        };

        // A typo for an entity that does not exist.
        let first = lookup("Tsamania");
        assert_eq!(first["found"], false);
        assert_eq!(first["query"], "Tsamania");

        // Asserted through the tool rather than by counting rows: if the
        // lookup had created the entity, this second call would find it. That
        // is exactly the symptom a user would hit, and it does not depend on
        // reaching past the server for a connection.
        assert_eq!(
            lookup("Tsamania")["found"],
            false,
            "a lookup must never write -- the second call found what the first created"
        );
    }

    /// A miss is a valid answer, not a tool error -- the reference returns an
    /// ordinary payload for it, and `isError` would make a client retry.
    #[test]
    fn test_an_unknown_entity_is_not_reported_as_a_tool_error() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let result = call(&server, "remind_me_entity", json!({ "name": "nobody" }));
        assert!(
            result.get("isError").is_none(),
            "a miss must not be an error, got: {}",
            result
        );
    }

    #[test]
    fn test_entity_lookup_returns_the_profile_for_a_known_entity() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(
            &server,
            "remind_me_add",
            json!({ "content": "Tasmania is an island", "entities": [{ "name": "Tasmania" }] }),
        );

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_entity",
            json!({
                "name": "tasmania"  // resolution is case-insensitive
            }),
        )))
        .unwrap();

        assert_eq!(report["found"], true);
        assert_eq!(report["entity"]["name"], "Tasmania");
        // Spread alongside the profile, not nested under it -- a caller
        // written against the reference reads `entity`/`memories` at the top
        // level.
        assert!(report.get("memories").is_some(), "got: {}", report);
        assert!(report.get("total_linked_memories").is_some());
    }

    /// The write survives the split, just under its own name.
    #[test]
    fn test_entity_upsert_still_creates() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        call(
            &server,
            "remind_me_entity_upsert",
            json!({ "name": "Hobart", "kind": "place" }),
        );

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_entity",
            json!({
                "name": "Hobart"
            }),
        )))
        .unwrap();
        assert_eq!(report["found"], true, "the upsert should have created it");
        assert_eq!(
            report["entity"]["kind"], "place",
            "and carried the kind through"
        );
    }

    /// Both halves must be declared, or a client cannot discover the write it
    /// used to get from `remind_me_entity`.
    #[test]
    fn test_both_entity_tools_are_declared_and_the_read_one_takes_a_limit() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap().clone();

        let read = tools
            .iter()
            .find(|t| t["name"] == "remind_me_entity")
            .expect("remind_me_entity must still be declared");
        let props = read["inputSchema"]["properties"].as_object().unwrap();
        assert!(props.contains_key("limit"), "the reference's field");
        assert!(
            !props.contains_key("kind"),
            "kind belongs to the write tool; `remind_me` rejects it here (extra=forbid)"
        );

        assert!(
            tools.iter().any(|t| t["name"] == "remind_me_entity_upsert"),
            "the write must stay reachable under its own name"
        );
    }

    /// Both tools defaulted to JSON; the reference defaults both to markdown
    /// (`models.py:510`, `models.py:754`). A caller who omits the field must
    /// get the same thing from either implementation.
    #[test]
    fn test_stats_defaults_to_markdown_and_honours_json() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(&server, "remind_me_add", json!({ "content": "a note" }));

        let markdown = text_of(&call(&server, "remind_me_stats", json!({})));
        assert!(
            markdown.starts_with("## Memory Store Statistics"),
            "the reference's heading, got: {}",
            markdown
        );
        assert!(markdown.contains("### Categories"), "got: {}", markdown);
        assert!(
            markdown.contains("### Recent Memories"),
            "got: {}",
            markdown
        );

        let raw = text_of(&call(
            &server,
            "remind_me_stats",
            json!({ "response_format": "json" }),
        ));
        let parsed: Value = serde_json::from_str(&raw).expect("json branch must parse");
        assert_eq!(parsed["total_memories"], 1);
    }

    #[test]
    fn test_history_defaults_to_markdown_and_honours_json() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let added: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_add",
            json!({
                "content": "the original wording"
            }),
        )))
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        // An update is what writes a revision.
        call(
            &server,
            "remind_me_update",
            json!({ "memory_id": id, "content": "the revised wording" }),
        );

        let markdown = text_of(&call(
            &server,
            "remind_me_history",
            json!({ "memory_id": id }),
        ));
        assert!(
            markdown.contains("revision(s) for memory"),
            "the reference's header, got: {}",
            markdown
        );
        assert!(
            markdown.contains("- **Revision `"),
            "the reference's bullet, got: {}",
            markdown
        );
        // The snapshot holds the *pre-edit* content -- what the edit replaced.
        assert!(
            markdown.contains("the original wording"),
            "got: {}",
            markdown
        );

        let raw = text_of(&call(
            &server,
            "remind_me_history",
            json!({ "memory_id": id, "response_format": "json" }),
        ));
        let parsed: Value = serde_json::from_str(&raw).expect("json branch must parse");
        // An envelope, matching the reference -- not the bare array this
        // used to return.
        assert_eq!(parsed["memory_id"], id);
        assert_eq!(parsed["count"], 1);
        assert!(parsed["revisions"].is_array(), "got: {}", parsed);
    }

    /// The empty case has its own sentence in the reference rather than an
    /// empty list, and a model reads that sentence.
    /// Pins the default so the next drift is caught rather than re-derived.
    /// The bounds already matched the reference; only this number had drifted
    /// (#183), and nothing was asserting it.
    #[test]
    fn test_history_limit_defaults_to_the_references_ten() {
        let parsed: HistoryInput =
            serde_json::from_value(json!({ "memory_id": "m1" })).expect("parse");
        assert_eq!(parsed.limit, 10);

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let declared = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "remind_me_history")
            .expect("remind_me_history")["inputSchema"]["properties"]["limit"]["default"]
            .clone();
        assert_eq!(
            declared, 10,
            "the declared schema must agree with the struct default"
        );
    }

    #[test]
    fn test_history_with_no_revisions_says_so_in_markdown() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let added: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_add",
            json!({
                "content": "never edited"
            }),
        )))
        .unwrap();
        let id = added["id"].as_str().unwrap();

        let markdown = text_of(&call(
            &server,
            "remind_me_history",
            json!({ "memory_id": id }),
        ));
        assert!(
            markdown.contains("_No revision history for memory"),
            "got: {}",
            markdown
        );
    }

    #[test]
    fn test_both_tools_declare_response_format_defaulting_to_markdown() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap().clone();

        for name in ["remind_me_stats", "remind_me_history"] {
            let tool = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("{} must be declared", name));
            let field = &tool["inputSchema"]["properties"]["response_format"];
            assert_eq!(field["default"], "markdown", "{} default", name);
            assert_eq!(field["type"], "string", "{} type", name);
        }
    }

    #[test]
    fn test_server_status_carries_the_webhook() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_server_status",
            json!({}),
        )))
        .unwrap();

        // Merged in by the MCP layer rather than gathered from the connection,
        // because that is where the endpoint's state lives.
        assert_eq!(report["webhook"]["enabled"], false);
        assert_eq!(report["sync_peer"]["enabled"], false);
        assert_eq!(report["sync"]["enabled"], false);
        assert_eq!(report["schema_current"], true);
        // No REMIND_ME_REMOTE_MCP in the test environment, so the remote
        // connector (FT-05, #85) reports disabled the same way the others do.
        assert_eq!(report["remote"]["enabled"], false);
        assert_eq!(report["remote"]["host"], "127.0.0.1");
        // #90: no backend configured, so embeddings is not-implemented --
        // and an in-memory DB has no on-disk location for a dashboard PID
        // file, so dashboard reports not-implemented too (not "not running",
        // which would claim a check happened when none could).
        assert_eq!(report["embeddings"]["state"], "not_implemented");
        assert_eq!(report["dashboard"]["state"], "not_implemented");
    }

    #[test]
    fn test_server_status_reports_embeddings_active_when_the_backend_is_configured_and_reachable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // A fake Ollama daemon answering the "ping" probe `available_embedder`
        // makes -- same shape as `ollama_embedder_test.rs`'s `fake_server`.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = serde_json::json!({ "embeddings": [[1.0, 0.0]] }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        std::env::set_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV, "ollama");
        std::env::set_var(
            remind_me_core::embedder::OLLAMA_URL_ENV,
            format!("http://127.0.0.1:{port}"),
        );
        std::env::set_var(remind_me_core::embedder::EMBEDDING_DIM_ENV, "2");

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_server_status",
            json!({}),
        )))
        .unwrap();

        std::env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
        std::env::remove_var(remind_me_core::embedder::OLLAMA_URL_ENV);
        std::env::remove_var(remind_me_core::embedder::EMBEDDING_DIM_ENV);
        handle.join().unwrap();

        assert_eq!(
            report["embeddings"]["state"], "active",
            "expected active, got {:?}",
            report["embeddings"]
        );
    }

    #[test]
    fn test_server_status_reports_dashboard_running_from_a_live_pid_file() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let dir = std::env::temp_dir().join(format!(
            "rrm_mcp_dashboard_status_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("memories.db");
        let db = Database::open(&db_path).unwrap();

        // A fake dashboard answering GET /health, exactly what
        // `pid::dashboard_status`'s live check probes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = r#"{"status":"ok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        let pid_path = remind_me_core::pid::pid_file_path(&db.conn()).unwrap();
        let record = remind_me_core::pid::write_pid_file(&pid_path, "127.0.0.1", port).unwrap();

        let server = McpServer::new(db);
        let report: Value = serde_json::from_str(&text_of(&call(
            &server,
            "remind_me_server_status",
            json!({}),
        )))
        .unwrap();

        handle.join().unwrap();
        remind_me_core::pid::remove_pid_file(&pid_path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(report["dashboard"]["running"], true);
        assert_eq!(report["dashboard"]["url"], record.url);
        assert_eq!(report["dashboard"]["pid"], record.pid);
    }

    /// The bug this guards: `remind_me_server_status`'s Markdown renderer
    /// looked for a `state` tag on every subsystem, but `dashboard`, `sync`,
    /// `webhook` and `remote` are overwritten at dispatch with their own
    /// untagged structs (see the JSON-mode test above) — so Markdown printed
    /// `?` for all four regardless of their real state, even though the JSON
    /// response for the same call carried the truth.
    #[test]
    fn test_server_status_markdown_reports_dashboard_state_instead_of_a_bare_question_mark() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let text = text_of(&call(
            &server,
            "remind_me_server_status",
            json!({"response_format": "markdown"}),
        ));

        assert!(
            text.contains("- dashboard: not running"),
            "expected a concrete dashboard line, got: {text}"
        );
        assert!(
            text.contains("- sync: disabled"),
            "expected a concrete sync line, got: {text}"
        );
        assert!(
            text.contains("- webhook: disabled"),
            "expected a concrete webhook line, got: {text}"
        );
        assert!(
            text.contains("- remote: disabled"),
            "expected a concrete remote line, got: {text}"
        );
        assert!(
            !text.contains(": ?"),
            "no subsystem line should be a bare '?': {text}"
        );
    }

    #[test]
    fn test_stopping_the_webhook_is_safe_when_none_is_running() {
        let db = Database::open_in_memory().unwrap();
        let mut server = McpServer::new(db);

        server.stop_webhook();
        server.stop_webhook();

        assert!(!server.webhook_status().running);
    }

    #[test]
    fn test_mcp_resources_and_prompts() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        let res_req = json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" });
        let res_resp = server.handle_request(&res_req.to_string()).unwrap();
        assert_eq!(res_resp["result"]["resources"][0]["uri"], "memory://stats");

        let prompt_req = json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list" });
        let prompt_resp = server.handle_request(&prompt_req.to_string()).unwrap();
        assert_eq!(
            prompt_resp["result"]["prompts"][0]["name"],
            "recall_context"
        );
    }

    /// `REMIND_ME_CODE_ROOTS` is process-global; distinct from `ENV_LOCK`
    /// above, which serializes a different variable
    /// (`REMIND_ME_EMBEDDING_BACKEND`) and would just add unrelated
    /// contention if reused here.
    static CODE_ROOTS_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn stale_candidates_returns_an_empty_list_when_unconfigured() {
        let _lock = CODE_ROOTS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(remind_me_core::code_refs::CODE_ROOTS_ENV);

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(
            &server,
            "remind_me_add",
            json!({ "content": "see /tmp/whatever.rs" }),
        );

        let res = call(&server, "remind_me_stale_candidates", json!({}));
        assert!(res.get("isError").is_none());
        assert!(text_of(&res).contains("\"count\": 0"));
    }

    #[test]
    fn stale_candidates_reports_a_deleted_anchor_end_to_end() {
        let _lock = CODE_ROOTS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("rrm_mcp_coderefs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("auth.rs");
        std::fs::write(&file, "fn login() {}\n").unwrap();
        std::env::set_var(
            remind_me_core::code_refs::CODE_ROOTS_ENV,
            root.display().to_string(),
        );

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        call(
            &server,
            "remind_me_add",
            json!({ "content": format!("don't touch {}", file.display()) }),
        );
        std::fs::remove_file(&file).unwrap();

        let res = call(&server, "remind_me_stale_candidates", json!({}));
        std::env::remove_var(remind_me_core::code_refs::CODE_ROOTS_ENV);
        let _ = std::fs::remove_dir_all(&root);

        assert!(res.get("isError").is_none());
        let text = text_of(&res);
        assert!(text.contains("\"count\": 1"), "{text}");
        assert!(text.contains("\"deleted\""), "{text}");
    }

    #[test]
    fn stale_candidates_response_reports_the_uncapped_total() {
        let _lock = CODE_ROOTS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("rrm_mcp_coderefs_total_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("auth.rs");
        std::fs::write(&file, "fn login() {}\n").unwrap();
        std::env::set_var(
            remind_me_core::code_refs::CODE_ROOTS_ENV,
            root.display().to_string(),
        );

        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        for i in 0..5 {
            call(
                &server,
                "remind_me_add",
                json!({ "content": format!("memo {i}: don't touch {}", file.display()) }),
            );
        }
        std::fs::remove_file(&file).unwrap();

        let res = call(&server, "remind_me_stale_candidates", json!({ "limit": 2 }));
        std::env::remove_var(remind_me_core::code_refs::CODE_ROOTS_ENV);
        let _ = std::fs::remove_dir_all(&root);

        assert!(res.get("isError").is_none());
        let body: Value = serde_json::from_str(&text_of(&res)).unwrap();
        assert_eq!(body["count"], 2, "the page stays capped at limit");
        assert_eq!(
            body["total_candidates"], 5,
            "total_candidates must be the real backlog, not just the page"
        );
    }

    #[test]
    fn stale_candidates_schema_bounds_match_the_rust_side_clamp_constants() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap().clone();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "remind_me_stale_candidates")
            .expect("remind_me_stale_candidates must still be declared");

        let limit_schema = &tool["inputSchema"]["properties"]["limit"];
        assert_eq!(
            limit_schema["minimum"].as_u64().unwrap() as usize,
            remind_me_core::STALE_CANDIDATES_LIMIT_MIN
        );
        assert_eq!(
            limit_schema["maximum"].as_u64().unwrap() as usize,
            remind_me_core::STALE_CANDIDATES_LIMIT_MAX
        );
    }

    #[test]
    fn promotion_candidates_schema_bounds_match_the_rust_side_clamp_constants() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);
        let listed = server
            .handle_request(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
            )
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap().clone();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "remind_me_promotion_candidates")
            .expect("remind_me_promotion_candidates must still be declared");

        let limit_schema = &tool["inputSchema"]["properties"]["limit"];
        assert_eq!(
            limit_schema["minimum"].as_u64().unwrap() as usize,
            remind_me_core::PROMOTION_LIMIT_MIN
        );
        assert_eq!(
            limit_schema["maximum"].as_u64().unwrap() as usize,
            remind_me_core::PROMOTION_LIMIT_MAX
        );
    }

    #[test]
    fn promotion_candidates_response_reports_an_uncapped_total_candidates_field() {
        let db = Database::open_in_memory().unwrap();
        let server = McpServer::new(db);

        // A capture that has never been decomposed is a capture_to_fact
        // candidate (see promotion::capture_candidates); one is enough to
        // prove the field is present and a real count, without needing to
        // exercise the fact_to_scenario/scenario_to_persona thresholds too.
        call(
            &server,
            "remind_me_auto_capture",
            json!({ "conversation": "raw transcript", "summary": "a summary" }),
        );

        let res = call(
            &server,
            "remind_me_promotion_candidates",
            json!({ "rung": "capture_to_fact" }),
        );
        assert!(res.get("isError").is_none());
        let body: Value = serde_json::from_str(&text_of(&res)).unwrap();
        assert!(
            body.get("total_candidates").is_some(),
            "total_candidates must be present in the response: {body}"
        );
    }
}

#[cfg(test)]
mod handshake_identity {
    use super::client_identity;
    use serde_json::json;

    fn parse(params: serde_json::Value) -> Option<String> {
        client_identity(&json!({ "method": "initialize", "params": params }))
    }

    #[test]
    fn name_and_version_are_joined() {
        // Both, because "which agent" and "which build of it" are different
        // questions and the interesting difference is often between releases.
        assert_eq!(
            parse(json!({ "clientInfo": { "name": "claude-code", "version": "2.1.0" } })),
            Some("claude-code/2.1.0".to_string())
        );
    }

    #[test]
    fn a_missing_version_still_yields_the_name() {
        assert_eq!(
            parse(json!({ "clientInfo": { "name": "claude-code" } })),
            Some("claude-code".to_string())
        );
        assert_eq!(
            parse(json!({ "clientInfo": { "name": "claude-code", "version": "  " } })),
            Some("claude-code".to_string())
        );
    }

    #[test]
    fn nothing_usable_yields_none_rather_than_an_empty_client() {
        // `None` matters: it makes `configured_client` fall through to the
        // configured value. An empty string would be recorded as the client
        // and would outrank a correctly-set REMIND_ME_CLIENT.
        assert_eq!(parse(json!({})), None);
        assert_eq!(parse(json!({ "clientInfo": {} })), None);
        assert_eq!(parse(json!({ "clientInfo": { "name": "" } })), None);
        assert_eq!(parse(json!({ "clientInfo": { "name": "   " } })), None);
        assert_eq!(parse(json!({ "clientInfo": { "name": 42 } })), None);
        // A handshake with no params at all must not panic.
        assert_eq!(client_identity(&json!({ "method": "initialize" })), None);
    }
}
