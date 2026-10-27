//! Tool groups, and the profiles that choose which groups a client sees.
//!
//! # What this buys
//!
//! **Context and selection.** Every advertised tool's name, description and
//! input schema is sent to the model on every session, and clients that defer
//! tools find them by keyword search over that text. Eleven tools a model can tell
//! apart beat seventy-eight it has to search through (ADR-0030).
//!
//! # Every tool is in exactly one group
//!
//! [`GROUPS`] is the whole public surface. A test asserts every tool the MCP
//! server advertises appears in exactly one group, so there is no implicit
//! bucket for a tool someone forgot to place. A name in no group and no alias
//! is still refused under any narrowed profile: the safe direction.
//!
//! # Aliases
//!
//! [`ALIASES`] are the names that merged into another tool. They stay callable
//! until 0.6.0 but are never listed, and an alias is allowed exactly when its
//! replacement is, so a hidden group stays hidden under its old names too.
//!
//! # Hidden means gone
//!
//! A hidden tool is absent from `tools/list` *and* refused on `tools/call`.
//! Merely undocumented would be worse than not having profiles: a model that
//! guessed the name would still reach it, and the caller would have no idea
//! their trimmed surface was porous.

use std::collections::BTreeSet;

pub const TOOL_PROFILE_ENV: &str = "REMIND_ME_TOOL_PROFILE";

/// Profile names that are not a group: `full` is every group, `standard` is
/// `core,pipeline,wiki` (what it covered before groups existed).
pub const PRESETS: [&str; 2] = ["full", "standard"];

/// What an ordinary session reaches for.
///
/// `remind_me_server_status` is here because it reports which profile is
/// active, and a profile you cannot diagnose from inside a session is a trap.
pub const CORE: [&str; 11] = [
    "remind_me_search",
    "remind_me_brief",
    "remind_me_get",
    "remind_me_add",
    "remind_me_update",
    "remind_me_delete",
    "remind_me_auto_capture",
    "remind_me_resolve",
    "remind_me_entity",
    "remind_me_digest",
    "remind_me_server_status",
];

/// The LLM-driven refinement loops: `queue` hands out work, the rest apply it.
pub const PIPELINE: [&str; 16] = [
    "remind_me_queue",
    "remind_me_decompose",
    "remind_me_annotate",
    "remind_me_normalize_apply",
    "remind_me_reclassify",
    "remind_me_promote",
    "remind_me_consolidate",
    "remind_me_feedback",
    "remind_me_entity_upsert",
    "remind_me_entity_traverse",
    "remind_me_persona",
    "remind_me_provenance",
    "remind_me_skeleton_read",
    "remind_me_skeleton_write",
    "remind_me_revert",
    // ADR-0027 (projects) suggests `core`; it stays here so core holds the
    // ten tools ADR-0030 settled on. An ordinary session can add `pipeline`.
    "remind_me_project",
];

pub const WIKI: [&str; 4] = [
    "remind_me_wiki_read",
    "remind_me_wiki_write",
    "remind_me_wiki_delete",
    "remind_me_wiki_compile",
];

pub const IMPORT: [&str; 3] = [
    "remind_me_import",
    "remind_me_undo_import",
    "remind_me_capture_transcript",
];

/// Operations a person runs, not a conversation. `self_update` leaves MCP for
/// the CLI in the next step of ADR-0030.
pub const ADMIN: [&str; 9] = [
    "remind_me_backup",
    "remind_me_reindex",
    "remind_me_archive_prune",
    "remind_me_export_memories",
    "remind_me_revoke_clients",
    "remind_me_api_key",
    "remind_me_sync_status",
    "remind_me_sync_repair",
    "remind_me_self_update",
];

pub const EXTRAS: [&str; 3] = [
    "remind_me_reminders",
    "remind_me_saved_searches",
    "remind_me_session_timeline",
];

/// Every public tool, by group, in the order `tools/list` advertises them.
pub const GROUPS: [(&str, &[&str]); 6] = [
    ("core", &CORE),
    ("pipeline", &PIPELINE),
    ("wiki", &WIKI),
    ("import", &IMPORT),
    ("admin", &ADMIN),
    ("extras", &EXTRAS),
];

/// Old name, and the public tool it merged into. Callable, never listed.
pub const ALIASES: [(&str, &str); 36] = [
    ("remind_me_list", "remind_me_search"),
    ("remind_me_references", "remind_me_search"),
    ("remind_me_get_capture", "remind_me_get"),
    ("remind_me_history", "remind_me_get"),
    ("remind_me_source", "remind_me_get"),
    ("remind_me_set_reminder", "remind_me_update"),
    ("remind_me_stats", "remind_me_digest"),
    ("remind_me_vitality_report", "remind_me_digest"),
    ("remind_me_watch_status", "remind_me_server_status"),
    ("remind_me_webhook_status", "remind_me_server_status"),
    ("remind_me_check_update", "remind_me_server_status"),
    ("remind_me_list_connectors", "remind_me_server_status"),
    ("remind_me_decompose_batch", "remind_me_queue"),
    ("remind_me_extract_batch", "remind_me_queue"),
    ("remind_me_normalize_batch", "remind_me_queue"),
    ("remind_me_reclassify_batch", "remind_me_queue"),
    ("remind_me_recalibrate_candidates", "remind_me_queue"),
    ("remind_me_contradiction_candidates", "remind_me_queue"),
    ("remind_me_promotion_candidates", "remind_me_queue"),
    ("remind_me_stale_candidates", "remind_me_queue"),
    ("remind_me_wiki_list", "remind_me_wiki_read"),
    ("remind_me_wiki_search", "remind_me_wiki_read"),
    ("remind_me_wiki_load", "remind_me_wiki_read"),
    ("remind_me_wiki_import", "remind_me_import"),
    ("remind_me_import_chat", "remind_me_import"),
    ("remind_me_import_directory", "remind_me_import"),
    ("remind_me_import_dbs", "remind_me_import"),
    ("remind_me_import_mempalace", "remind_me_import"),
    ("remind_me_sync_reconcile", "remind_me_sync_status"),
    ("remind_me_sync_reconcile_peer", "remind_me_sync_status"),
    ("remind_me_list_reminders", "remind_me_reminders"),
    ("remind_me_reminders_ics_url", "remind_me_reminders"),
    ("remind_me_save_search", "remind_me_saved_searches"),
    ("remind_me_list_saved_searches", "remind_me_saved_searches"),
    ("remind_me_run_saved_search", "remind_me_saved_searches"),
    ("remind_me_delete_saved_search", "remind_me_saved_searches"),
];

/// Prompts driving [`PIPELINE`], hidden unless that group is on.
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

/// The public tool an alias merged into, or `None` for a name that is not one.
pub fn replacement(alias: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find(|(old, _)| *old == alias)
        .map(|(_, new)| *new)
}

/// The group `tool` belongs to, through its replacement for an alias.
pub fn group_of(tool: &str) -> Option<&'static str> {
    let public = replacement(tool).unwrap_or(tool);
    GROUPS
        .iter()
        .find(|(_, tools)| tools.contains(&public))
        .map(|(group, _)| *group)
}

/// The configured profile, normalised, falling back to `full` on anything
/// unrecognised.
///
/// A profile is `full`, `standard`, or a comma list of [`GROUPS`] names such
/// as `core,wiki`. A typo yields the *widest* surface rather than an error or
/// an empty one: failing to start over a misspelled optimisation would be
/// worse than the misspelling, and an empty surface would look like a broken
/// server.
pub fn configured_profile() -> String {
    // Per session: on the daemon, each client keeps its own profile.
    let raw = crate::daemon::session::var(TOOL_PROFILE_ENV).unwrap_or_default();
    normalise(&raw).unwrap_or_else(|| {
        eprintln!(
            "unknown {}={:?}; falling back to 'full'. Valid: {}, or a comma list of {}",
            TOOL_PROFILE_ENV,
            raw.trim(),
            PRESETS.join(", "),
            group_names().join(", ")
        );
        "full".to_string()
    })
}

/// `raw` lowercased with blanks removed, or `None` when a part is unknown.
/// Unset or blank is `full`.
fn normalise(raw: &str) -> Option<String> {
    let parts: Vec<String> = raw
        .split(',')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Some("full".to_string());
    }
    let known = |p: &String| PRESETS.contains(&p.as_str()) || group_names().contains(&p.as_str());
    parts.iter().all(known).then(|| parts.join(","))
}

fn group_names() -> Vec<&'static str> {
    GROUPS.iter().map(|(group, _)| *group).collect()
}

/// The groups `profile` turns on, or `None` when it is `full`. Core is always
/// on: a profile without search and add is not a narrower memory, it is none.
pub fn allowed_groups(profile: &str) -> Option<BTreeSet<&'static str>> {
    let mut groups = BTreeSet::from(["core"]);
    for part in profile.split(',').map(str::trim) {
        match part {
            "full" => return None,
            "standard" => groups.extend(["pipeline", "wiki"]),
            other => {
                if let Some(group) = group_names().into_iter().find(|g| *g == other) {
                    groups.insert(group);
                }
            }
        }
    }
    Some(groups)
}

/// Public tool names `profile` advertises, or `None` when everything is.
pub fn allowed_tools(profile: &str) -> Option<BTreeSet<&'static str>> {
    let groups = allowed_groups(profile)?;
    Some(
        GROUPS
            .iter()
            .filter(|(group, _)| groups.contains(group))
            .flat_map(|(_, tools)| tools.iter().copied())
            .collect(),
    )
}

/// Whether `tool` (a public name or an alias) is callable under `profile`.
pub fn tool_allowed(profile: &str, tool: &str) -> bool {
    match (allowed_groups(profile), group_of(tool)) {
        (None, _) => true,
        (Some(groups), Some(group)) => groups.contains(group),
        (Some(_), None) => false,
    }
}

/// Whether `prompt` is offered under `profile`.
pub fn prompt_allowed(profile: &str, prompt: &str) -> bool {
    !MAINTENANCE_PROMPTS.contains(&prompt)
        || allowed_groups(profile).is_none_or(|groups| groups.contains("pipeline"))
}
