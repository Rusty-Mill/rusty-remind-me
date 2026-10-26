//! Coverage for tool groups and profiles (gap E3, issue #122; ADR-0027).
//!
//! That every grouped name is a tool the server actually advertises is
//! asserted at the protocol surface, in `remind_me_mcp`'s own tests.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::tool_profiles::{
    allowed_groups, allowed_tools, configured_profile, group_of, prompt_allowed, replacement,
    tool_allowed, ALIASES, CORE, GROUPS, MAINTENANCE_PROMPTS, PIPELINE, TOOL_PROFILE_ENV, WIKI,
};
use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn profile_for(raw: &str) -> String {
    crate::test_env::set_var(TOOL_PROFILE_ENV, raw);
    let profile = configured_profile();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
    profile
}

#[test]
fn the_default_profile_is_full() {
    let _guard = env_lock();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);

    // Upgrading must change nothing for an existing install (ADR-0027 flips
    // this in its second step, not this one).
    assert_eq!(configured_profile(), "full");
    assert!(allowed_tools("full").is_none(), "full allows everything");
}

#[test]
fn an_unknown_profile_falls_back_to_full_rather_than_failing() {
    let _guard = env_lock();
    // A typo yields the widest surface. Refusing to start over a misspelled
    // optimisation would be worse than the misspelling, and an empty surface
    // would look like a broken server.
    assert_eq!(profile_for("minimal"), "full");
    assert_eq!(
        profile_for("core,wikki"),
        "full",
        "one bad part spoils the list"
    );
}

#[test]
fn profiles_are_normalised_for_case_blanks_and_spacing() {
    let _guard = env_lock();
    assert_eq!(profile_for("  Core \n"), "core");
    assert_eq!(profile_for(" core , WIKI ,"), "core,wiki");
    assert_eq!(profile_for("   "), "full");
}

#[test]
fn core_is_ten_tools_and_always_on() {
    assert_eq!(CORE.len(), 10);
    let core = allowed_tools("core").expect("core narrows");
    assert_eq!(core, CORE.into_iter().collect());
    // Naming another group never drops core.
    let wiki = allowed_tools("wiki").expect("wiki narrows");
    assert!(CORE.iter().chain(WIKI.iter()).all(|t| wiki.contains(t)));
    assert_eq!(wiki.len(), CORE.len() + WIKI.len());
}

#[test]
fn standard_is_core_pipeline_and_wiki() {
    assert_eq!(
        allowed_groups("standard"),
        Some(BTreeSet::from(["core", "pipeline", "wiki"]))
    );
    assert_eq!(
        allowed_groups("core,pipeline,wiki"),
        allowed_groups("standard")
    );
    assert!(
        allowed_groups("core,full").is_none(),
        "full anywhere is everything"
    );
}

#[test]
fn every_public_tool_is_in_exactly_one_group() {
    let mut seen = BTreeSet::new();
    for (group, tools) in GROUPS {
        for tool in tools {
            assert!(seen.insert(*tool), "{tool} is in more than one group");
            assert_eq!(group_of(tool), Some(group));
        }
    }
    assert_eq!(seen.len(), 43);
}

#[test]
fn every_alias_points_at_a_public_tool_and_is_not_one_itself() {
    let public: BTreeSet<&str> = GROUPS.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    let mut seen = BTreeSet::new();
    for (old, new) in ALIASES {
        assert!(seen.insert(old), "{old} aliased twice");
        assert!(
            public.contains(new),
            "{old} points at {new}, which is not public"
        );
        assert!(!public.contains(old), "{old} is both public and an alias");
        assert_eq!(replacement(old), Some(new));
    }
    // 75 tools before ADR-0027: 43 public names, 4 of them new, plus 36 aliases.
    assert_eq!(public.len() - 4 + ALIASES.len(), 75);
}

#[test]
fn an_alias_is_allowed_exactly_when_its_replacement_is() {
    // stats merged into digest (core); import_chat into import (import).
    assert!(tool_allowed("core", "remind_me_stats"));
    assert!(!tool_allowed("core", "remind_me_import_chat"));
    assert!(tool_allowed("core,import", "remind_me_import_chat"));
    assert!(!tool_allowed("standard", "remind_me_sync_reconcile"));
    assert!(tool_allowed("full", "remind_me_sync_reconcile"));
}

#[test]
fn a_name_in_no_group_is_hidden_whenever_the_profile_narrows() {
    assert!(!tool_allowed("core", "remind_me_some_future_tool"));
    assert!(!tool_allowed("standard", "remind_me_some_future_tool"));
    assert!(tool_allowed("full", "remind_me_some_future_tool"));
}

#[test]
fn server_status_stays_visible_in_every_profile() {
    // It reports which profile is active, and a profile you cannot diagnose
    // from inside a session is a trap.
    for profile in ["core", "wiki", "standard", "full", "admin"] {
        assert!(
            tool_allowed(profile, "remind_me_server_status"),
            "{profile}"
        );
    }
}

#[test]
fn maintenance_prompts_follow_the_pipeline_group() {
    assert!(PIPELINE.contains(&"remind_me_queue"));
    for prompt in MAINTENANCE_PROMPTS {
        assert!(
            !prompt_allowed("core", prompt),
            "{prompt} offered under core"
        );
        assert!(!prompt_allowed("core,wiki", prompt));
        assert!(prompt_allowed("core,pipeline", prompt));
        assert!(prompt_allowed("standard", prompt));
        assert!(prompt_allowed("full", prompt));
    }
    assert!(prompt_allowed("core", "recall_context"));
}
