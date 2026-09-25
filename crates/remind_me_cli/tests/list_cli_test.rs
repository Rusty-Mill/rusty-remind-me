//! End-to-end coverage for the `list` subcommand (issue #167).
//!
//! The unit tests in `main.rs` cover flag parsing in isolation. These run the
//! real binary against a real database, which is the only way to reach the
//! dispatch arm itself — `main` is not callable from a test, so a parser that
//! is correct but wired to nothing would still pass there.

use std::path::Path;
use std::process::Command;

/// Run the CLI against a scratch database and return `(stdout, stderr, ok)`.
fn run(db: &Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_rusty-remind-me"))
        .args(args)
        .env("REMIND_ME_DB_PATH", db)
        // No update-check suppression needed: `updater::start_background_check`
        // is inside `main`'s server branch, so `list`/`add` never reach it and
        // these tests do not touch the network.
        .output()
        .expect("the CLI binary should be runnable");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn scratch_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm-list-test-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join("remind_me.db")
}

#[test]
fn an_empty_store_lists_cleanly_rather_than_erroring() {
    let db = scratch_db("empty");
    let (stdout, stderr, ok) = run(&db, &["list"]);
    assert!(
        ok,
        "`list` on an empty store should exit 0. stderr: {}",
        stderr
    );
    // Matches the reference's `_fmt_memories` empty branch.
    assert!(
        stdout.contains("_No memories found._"),
        "expected the reference's empty-list text, got: {}",
        stdout
    );
}

#[test]
fn an_empty_store_lists_cleanly_as_json_too() {
    let db = scratch_db("empty-json");
    let (stdout, stderr, ok) = run(&db, &["list", "--json"]);
    assert!(ok, "`list --json` should exit 0. stderr: {}", stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json must emit parseable JSON");
    assert_eq!(parsed["total"], 0);
    assert_eq!(parsed["memories"].as_array().map(Vec::len), Some(0));
}

#[test]
fn an_added_memory_comes_back_from_list() {
    let db = scratch_db("happy");
    let (_, stderr, ok) = run(&db, &["add", "the kitchen tap drips"]);
    assert!(ok, "`add` should succeed. stderr: {}", stderr);

    let (stdout, stderr, ok) = run(&db, &["list"]);
    assert!(ok, "`list` should succeed. stderr: {}", stderr);
    assert!(
        stdout.contains("the kitchen tap drips"),
        "the added memory should be listed, got: {}",
        stdout
    );
    // The markdown renderer is shared with the reminders surface; assert the
    // header is present so a swap to a bare content dump would be caught.
    assert!(
        stdout.contains("### Memory `"),
        "expected the reference's markdown block header, got: {}",
        stdout
    );
}

#[test]
fn a_category_filter_excludes_non_matching_rows() {
    let db = scratch_db("filter");
    run(&db, &["add", "a general note"]);

    // `add` writes category "general", so a different category must match
    // nothing -- proving the filter is actually applied rather than ignored.
    let (stdout, _, ok) = run(&db, &["list", "--category", "preference", "--json"]);
    assert!(ok);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("parseable JSON");
    assert_eq!(
        parsed["total"], 0,
        "the filter should exclude the general row"
    );

    let (stdout, _, ok) = run(&db, &["list", "--category", "general", "--json"]);
    assert!(ok);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("parseable JSON");
    assert_eq!(parsed["total"], 1, "the matching category should return it");
}

#[test]
fn an_invalid_limit_exits_non_zero_with_a_clean_message() {
    let db = scratch_db("badlimit");
    let (_, stderr, ok) = run(&db, &["list", "--limit", "0"]);
    assert!(!ok, "an out-of-range --limit should be refused");
    assert!(
        stderr.contains("must be between"),
        "expected a range message, got: {}",
        stderr
    );
    // A CLI boundary must never leak a panic message to the user.
    assert!(
        !stderr.contains("panicked"),
        "the error path must not panic, got: {}",
        stderr
    );
}

#[test]
fn limit_actually_bounds_the_returned_page() {
    let db = scratch_db("limit");
    for note in ["first note", "second note", "third note"] {
        run(&db, &["add", note]);
    }
    let (stdout, _, ok) = run(&db, &["list", "--limit", "2", "--json"]);
    assert!(ok);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("parseable JSON");
    assert_eq!(
        parsed["memories"].as_array().map(Vec::len),
        Some(2),
        "the page should honour --limit"
    );
    // `total` counts every match, not just the page -- that separation is what
    // lets a caller paginate without a second round trip.
    assert_eq!(parsed["total"], 3, "total should count all matches");
}
