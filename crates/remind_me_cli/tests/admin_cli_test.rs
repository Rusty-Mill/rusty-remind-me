//! `rusty-remind-me admin <verb>` (ADR-0030 step 2): the admin tool group left
//! the default MCP surface, so a person reaches it here. These run the real
//! binary against a scratch store, with the narrowest profile set, to show the
//! CLI does not depend on what an MCP client is allowed to see.

use std::path::Path;
use std::process::Command;

/// Run `admin <args>` in `dir` and return `(stdout, stderr, ok)`.
fn admin(dir: &Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_rusty-remind-me"))
        .arg("admin")
        .args(args)
        .env("HOME", dir)
        .env("REMIND_ME_DB_PATH", dir.join("remind_me.db"))
        .env("REMIND_ME_REMOTE_OAUTH_STATE_FILE", dir.join("oauth.json"))
        // The daemon is on by default; one started here would outlive the test.
        .env("REMIND_ME_DAEMON", "0")
        .env("REMIND_ME_TOOL_PROFILE", "core")
        .output()
        .expect("the CLI binary should be runnable");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm-admin-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn every_admin_verb_runs_against_a_temp_store_under_the_core_profile() {
    let dir = scratch("verbs");
    for verb in [
        "backup",
        "reindex",
        "archive-prune",
        "export-memories",
        "revoke-clients",
        "api-key",
    ] {
        let (stdout, stderr, ok) = admin(&dir, &[verb]);
        assert!(ok, "{verb} failed: {stderr}");
        assert!(!stdout.trim().is_empty(), "{verb} printed nothing");
        assert!(!stdout.contains("not available under"), "{verb}: {stdout}");
    }
}

#[test]
fn a_verb_takes_the_tools_json_arguments() {
    let dir = scratch("args");
    let (stdout, stderr, ok) = admin(&dir, &["api-key", r#"{"action":"issue","name":"ci"}"#]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("ci"), "{stdout}");
    let (stdout, _, ok) = admin(&dir, &["api-key", r#"{"action":"list"}"#]);
    assert!(ok && stdout.contains("ci"), "{stdout}");
}

#[test]
fn a_tool_error_is_a_failing_exit() {
    let dir = scratch("tool-error");
    let (stdout, _, ok) = admin(&dir, &["api-key", r#"{"action":"issue"}"#]);
    assert!(!ok, "issuing a key without a name should fail");
    assert!(stdout.contains("name is required"), "{stdout}");
}

#[test]
fn an_unknown_verb_and_bad_arguments_are_refused_with_usage() {
    let dir = scratch("refused");
    for args in [
        &["bogus"][..],
        &[][..],
        // Not an admin verb, even though it is a tool.
        &["search"][..],
        &["backup", "{not json"][..],
    ] {
        let (_, stderr, ok) = admin(&dir, args);
        assert!(!ok, "{args:?} should fail");
        assert!(stderr.contains("Usage: rusty-remind-me admin"), "{stderr}");
    }
}
