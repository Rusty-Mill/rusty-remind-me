//! Profiles narrowing which tools the MCP server advertises.
//!
//! # What this buys, and what it does not
//!
//! It buys **context**. Every advertised tool's name, description and input
//! schema is sent to the model on every session, and this crate is past 60
//! tools. Nothing else.
//!
//! It is emphatically **not** a fix for tool-selection accuracy. The tools that
//! genuinely compete — `search`, `list`, `get`, `entity`, all of which read as
//! "find things" — are every one of them in [`CORE`], so no profile separates
//! them. Anyone reaching for a profile hoping the model will stop confusing
//! those four will be disappointed, and it is better to say so than to let the
//! feature imply otherwise.
//!
//! # Unknown tools default to the most restricted tier
//!
//! [`allowed_tools`] treats anything outside `CORE` and `MAINTENANCE` as
//! admin/ops. A tool added later therefore starts hidden under a narrowed
//! profile rather than smuggling itself onto a surface someone deliberately
//! trimmed — the safe direction for a list that will keep growing.
//!
//! # Hidden means gone
//!
//! A hidden tool is absent from `tools/list` *and* refused on `tools/call`.
//! Merely undocumented would be worse than not having profiles: a model that
//! guessed the name would still reach it, and the caller would have no idea
//! their trimmed surface was porous.

use std::collections::BTreeSet;

pub const TOOL_PROFILE_ENV: &str = "REMIND_ME_TOOL_PROFILE";
pub const VALID_PROFILES: [&str; 3] = ["full", "standard", "core"];

/// The conversational surface: what an ordinary session actually reaches for.
///
/// `remind_me_server_status` is here deliberately despite otherwise being an
/// ops tool — it is what *reports which profile is active*, and a profile you
/// cannot diagnose from inside a session is a trap.
pub const CORE: [&str; 18] = [
    "remind_me_search",
    "remind_me_add",
    "remind_me_get",
    "remind_me_list",
    "remind_me_update",
    "remind_me_delete",
    "remind_me_entity",
    // Target-only. In `core` because `remind_me_entity` used to write and
    // was already here -- splitting read from write must not quietly cost a
    // trimmed profile the ability to create an entity at all.
    "remind_me_entity_upsert",
    "remind_me_entity_traverse",
    "remind_me_feedback",
    "remind_me_auto_capture",
    "remind_me_get_capture",
    "remind_me_stats",
    "remind_me_server_status",
    "remind_me_wiki_load",
    "remind_me_wiki_read",
    "remind_me_wiki_search",
    "remind_me_wiki_list",
];

/// The LLM-driven maintenance loops, each fronted by a prompt.
pub const MAINTENANCE: [&str; 15] = [
    "remind_me_decompose",
    "remind_me_decompose_batch",
    "remind_me_normalize_batch",
    "remind_me_normalize_apply",
    "remind_me_extract_batch",
    "remind_me_annotate",
    "remind_me_reclassify",
    "remind_me_reclassify_batch",
    "remind_me_recalibrate_candidates",
    "remind_me_contradiction_candidates",
    "remind_me_consolidate",
    "remind_me_vitality_report",
    "remind_me_wiki_write",
    "remind_me_wiki_compile",
    "remind_me_wiki_delete",
];

/// Prompts driving [`MAINTENANCE`], hidden alongside it under `core`.
///
/// A prompt that sequences tools the session cannot see is worse than an
/// absent one: it walks the model into calls that will be refused.
pub const MAINTENANCE_PROMPTS: [&str; 8] = [
    "decompose_facts",
    "normalize_imports",
    "backfill_graph",
    "classify_memories",
    "compile_wiki",
    "consolidate_duplicates",
    "recalibrate_importance",
    "review_contradictions",
];

/// The configured profile, falling back to `full` on anything unrecognised.
///
/// A typo yields the *widest* surface rather than an error or an empty one:
/// failing to start over a misspelled optimisation would be worse than the
/// misspelling, and an empty surface would look like a broken server.
pub fn configured_profile() -> String {
    let raw = std::env::var(TOOL_PROFILE_ENV)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if raw.is_empty() {
        return "full".to_string();
    }
    if VALID_PROFILES.contains(&raw.as_str()) {
        raw
    } else {
        eprintln!(
            "unknown {}={:?}; falling back to 'full'. Valid: {}",
            TOOL_PROFILE_ENV,
            raw,
            VALID_PROFILES.join(", ")
        );
        "full".to_string()
    }
}

/// Tool names this profile advertises, or `None` when everything is allowed.
pub fn allowed_tools(profile: &str) -> Option<BTreeSet<&'static str>> {
    match profile {
        "core" => Some(CORE.into_iter().collect()),
        "standard" => Some(CORE.into_iter().chain(MAINTENANCE).collect()),
        _ => None,
    }
}

/// Whether `tool` is advertised and callable under `profile`.
pub fn tool_allowed(profile: &str, tool: &str) -> bool {
    match allowed_tools(profile) {
        None => true,
        Some(allowed) => allowed.contains(tool),
    }
}

/// Whether `prompt` is offered under `profile`.
pub fn prompt_allowed(profile: &str, prompt: &str) -> bool {
    profile != "core" || !MAINTENANCE_PROMPTS.contains(&prompt)
}
