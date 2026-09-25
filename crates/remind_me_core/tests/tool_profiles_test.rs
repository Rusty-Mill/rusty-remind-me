//! Coverage for tool profiles (gap E3, issue #122).
//!
//! The tier tables are asserted against the names this crate actually
//! advertises, not just for internal consistency: a tier listing a tool that
//! does not exist silently shrinks the profile, and neither the table nor the
//! server would complain.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::tool_profiles::{
    allowed_tools, configured_profile, prompt_allowed, tool_allowed, CORE, MAINTENANCE,
    MAINTENANCE_PROMPTS, TOOL_PROFILE_ENV, VALID_PROFILES,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[test]
fn the_default_profile_is_full() {
    let _guard = env_lock();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);

    // Upgrading must change nothing for an existing install.
    assert_eq!(configured_profile(), "full");
    assert!(allowed_tools("full").is_none(), "full allows everything");
}

#[test]
fn an_unknown_profile_falls_back_to_full_rather_than_failing() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "minimal");

    // A typo yields the widest surface. Refusing to start over a misspelled
    // optimisation would be worse than the misspelling, and an empty surface
    // would look like a broken server.
    assert_eq!(configured_profile(), "full");
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn profile_names_are_case_and_whitespace_insensitive() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "  Core \n");
    assert_eq!(configured_profile(), "core");
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn standard_is_core_plus_maintenance_and_core_is_neither_more_nor_less() {
    let core = allowed_tools("core").expect("core narrows");
    let standard = allowed_tools("standard").expect("standard narrows");

    assert_eq!(core.len(), CORE.len());
    assert_eq!(standard.len(), CORE.len() + MAINTENANCE.len());
    for name in CORE {
        assert!(core.contains(name), "{name} missing from core");
        assert!(standard.contains(name), "{name} missing from standard");
    }
    for name in MAINTENANCE {
        assert!(!core.contains(name), "{name} leaked into core");
        assert!(standard.contains(name), "{name} missing from standard");
    }
}

#[test]
fn the_tiers_do_not_overlap() {
    // A name in both tables is a table nobody has read carefully, and the
    // overlap decides nothing — it just hides which tier was intended.
    for name in MAINTENANCE {
        assert!(!CORE.contains(&name), "{name} is in both tiers");
    }
}

#[test]
fn an_unlisted_tool_is_treated_as_admin_and_hidden_when_narrowed() {
    // The default matters more than any individual assignment: a tool added
    // later must start hidden under a narrowed profile rather than smuggling
    // itself onto a surface someone deliberately trimmed.
    assert!(!tool_allowed("core", "remind_me_some_future_tool"));
    assert!(!tool_allowed("standard", "remind_me_some_future_tool"));
    assert!(tool_allowed("full", "remind_me_some_future_tool"));
}

/// Ops tools sampled by the hiding test. Checked for existence, because
/// "an unlisted tool is hidden" is already true of a typo.
const OPS_SAMPLES: [&str; 4] = [
    "remind_me_import_chat",
    "remind_me_sync_status",
    "remind_me_backup",
    "remind_me_api_key",
];

#[test]
fn ops_tools_are_hidden_under_standard_and_core() {
    // Real advertised names, asserted below to still exist — naming a tool
    // that does not exist would make this test pass for the wrong reason,
    // since anything unlisted is hidden by default anyway.
    for name in OPS_SAMPLES {
        assert!(!tool_allowed("core", name), "{name} visible under core");
        assert!(
            !tool_allowed("standard", name),
            "{name} visible under standard"
        );
        assert!(tool_allowed("full", name));
    }
}

#[test]
fn server_status_stays_visible_in_every_profile() {
    // It is otherwise an ops tool, but it is what reports which profile is
    // active — and a profile you cannot diagnose from inside a session is a
    // trap.
    for profile in VALID_PROFILES {
        assert!(
            tool_allowed(profile, "remind_me_server_status"),
            "server_status hidden under {profile}"
        );
    }
}

#[test]
fn core_hides_the_maintenance_prompts_and_the_others_do_not() {
    for prompt in MAINTENANCE_PROMPTS {
        assert!(
            !prompt_allowed("core", prompt),
            "{prompt} offered under core"
        );
        assert!(prompt_allowed("standard", prompt));
        assert!(prompt_allowed("full", prompt));
    }
}

#[test]
fn a_non_maintenance_prompt_survives_every_profile() {
    for profile in VALID_PROFILES {
        assert!(prompt_allowed(profile, "recall_context"));
    }
}

#[test]
fn every_tiered_name_is_a_tool_this_crate_actually_advertises() {
    // A tier naming a tool that does not exist silently shrinks the profile
    // by one and nothing complains — not the table, not the server, not a
    // consistency check that only compares the two tiers against each other.
    let source = include_str!("../../remind_me_mcp/src/lib.rs");
    let advertised: Vec<&str> = source
        .match_indices("\"name\": \"remind_me_")
        .filter_map(|(at, _)| {
            let rest = &source[at + "\"name\": \"".len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .collect();

    for name in CORE.into_iter().chain(MAINTENANCE) {
        assert!(
            advertised.contains(&name),
            "{name} is tiered but not advertised — the profile is one tool smaller than it looks"
        );
    }

    // And the ops names the hiding test samples: a typo there passes for the
    // wrong reason, because anything unlisted is hidden by default.
    for name in OPS_SAMPLES {
        assert!(
            advertised.contains(&name),
            "{name} is sampled as an ops tool but is not advertised at all"
        );
    }
}
