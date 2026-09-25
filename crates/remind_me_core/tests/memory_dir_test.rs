//! Where per-user files other than the database live (issue #228).
//!
//! `resolve_memory_dir_from` is exercised the same way `db_path_test.rs`
//! exercises `resolve_db_path_from` — a fake, never-touched home path is
//! fine, since it only builds a `PathBuf` and never looks at the
//! filesystem.
//!
//! `resolve_memory_dir_child_from`'s legacy-directory fallback is different:
//! its whole job is deciding between two paths based on which one actually
//! *exists*, so those tests build real files under a real scratch directory
//! instead. `std::env::set_var` is still avoided for the same
//! process-global/racing-threads reason `db_path_test.rs` gives.

use remind_me_core::db::{
    resolve_memory_dir_child_from, resolve_memory_dir_from, DEFAULT_DIR_NAME, MCP_DIR_ENV,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

fn fake_home() -> PathBuf {
    PathBuf::from("/home/tester")
}

/// A real, empty scratch directory standing in for `home`, unique per test
/// via `name` and the process id (matches this crate's other `scratch()`
/// helpers, e.g. `dbs_import_test.rs`).
fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm_memory_dir_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// resolve_memory_dir_from — pure, no filesystem
// ---------------------------------------------------------------------------

#[test]
fn nothing_set_is_the_hyphenated_default() {
    let dir = resolve_memory_dir_from(env(&[]), fake_home());
    assert_eq!(dir, fake_home().join(DEFAULT_DIR_NAME));
    assert_eq!(dir, PathBuf::from("/home/tester/.remind-me"));
}

#[test]
fn mcp_dir_env_wins_and_is_tilde_expanded() {
    let dir = resolve_memory_dir_from(env(&[(MCP_DIR_ENV, "~/custom-dir")]), fake_home());
    assert_eq!(dir, PathBuf::from("/home/tester/custom-dir"));
}

#[test]
fn a_blank_mcp_dir_env_falls_through_to_the_default() {
    let dir = resolve_memory_dir_from(env(&[(MCP_DIR_ENV, "  ")]), fake_home());
    assert_eq!(dir, fake_home().join(DEFAULT_DIR_NAME));
}

#[test]
fn resolve_db_path_and_resolve_memory_dir_agree_on_the_default_directory() {
    // The whole point of lifting the directory half out of resolve_db_path:
    // one setting, one directory, for every per-user file this crate writes.
    let memory_dir = resolve_memory_dir_from(env(&[]), fake_home());
    let db_path = remind_me_core::db::resolve_db_path_from(env(&[]), fake_home());
    assert_eq!(db_path, memory_dir.join(remind_me_core::db::DB_FILE_NAME));
}

// ---------------------------------------------------------------------------
// resolve_memory_dir_child_from — the #228 legacy-directory fallback
// ---------------------------------------------------------------------------

#[test]
fn neither_location_exists_resolves_to_the_new_default() {
    let home = scratch_home("neither");
    let resolved = resolve_memory_dir_child_from(env(&[]), home.clone(), "wiki");
    assert_eq!(resolved, home.join(".remind-me").join("wiki"));
}

#[test]
fn only_the_new_hyphenated_location_exists() {
    let home = scratch_home("new_only");
    let new_dir = home.join(".remind-me");
    std::fs::create_dir_all(new_dir.join("wiki")).unwrap();

    let resolved = resolve_memory_dir_child_from(env(&[]), home.clone(), "wiki");
    assert_eq!(resolved, new_dir.join("wiki"));
}

#[test]
fn only_the_legacy_underscored_location_exists_falls_back_to_it() {
    // The actual bug (#228): a user upgraded, the new hyphenated directory
    // was never populated, and their wiki/API keys/ICS token are still
    // sitting under the old underscored one. Silently pointing a fresh
    // `~/.remind-me/wiki` at nothing would look like the wiki was wiped.
    let home = scratch_home("legacy_only");
    let legacy_dir = home.join(".remind_me");
    std::fs::create_dir_all(legacy_dir.join("wiki")).unwrap();

    let resolved = resolve_memory_dir_child_from(env(&[]), home.clone(), "wiki");
    assert_eq!(resolved, legacy_dir.join("wiki"));
}

#[test]
fn both_locations_exist_the_new_one_wins() {
    let home = scratch_home("both");
    std::fs::create_dir_all(home.join(".remind-me").join("wiki")).unwrap();
    std::fs::create_dir_all(home.join(".remind_me").join("wiki")).unwrap();

    let resolved = resolve_memory_dir_child_from(env(&[]), home.clone(), "wiki");
    assert_eq!(resolved, home.join(".remind-me").join("wiki"));
}

#[test]
fn an_explicit_mcp_dir_override_opts_out_of_the_legacy_fallback_entirely() {
    // A custom directory has no "legacy" counterpart under `home` to fall
    // back to -- the fallback is specifically for the *default* location's
    // rename, not a general "does this file exist anywhere" search.
    let home = scratch_home("override");
    std::fs::create_dir_all(home.join(".remind_me").join("wiki")).unwrap();
    let custom_dir = home.join("custom-dir");
    std::fs::create_dir_all(&custom_dir).unwrap();

    let resolved = resolve_memory_dir_child_from(
        env(&[(MCP_DIR_ENV, custom_dir.display().to_string().as_str())]),
        home,
        "wiki",
    );
    assert_eq!(resolved, custom_dir.join("wiki"));
}

#[test]
fn a_blank_mcp_dir_override_does_not_opt_out_of_the_fallback() {
    let home = scratch_home("blank_override");
    std::fs::create_dir_all(home.join(".remind_me").join("wiki")).unwrap();

    let resolved = resolve_memory_dir_child_from(env(&[(MCP_DIR_ENV, "  ")]), home.clone(), "wiki");
    assert_eq!(resolved, home.join(".remind_me").join("wiki"));
}

#[test]
fn works_for_a_plain_file_not_just_a_directory() {
    let home = scratch_home("file");
    let legacy_dir = home.join(".remind_me");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(legacy_dir.join("ics_token"), b"old-token").unwrap();

    let resolved = resolve_memory_dir_child_from(env(&[]), home, "ics_token");
    assert_eq!(resolved, legacy_dir.join("ics_token"));
    assert_eq!(std::fs::read(&resolved).unwrap(), b"old-token");
}

// ---------------------------------------------------------------------------
// Regression guard (#228's own acceptance criterion): no new `.remind_me`
// (underscore) literal anywhere under crates/*/src/, outside the one place
// this file's own fallback is allowed to name it.
// ---------------------------------------------------------------------------

/// The only source location the quoted literal `".remind_me"` may appear:
/// `resolve_memory_dir_child`'s own read-only fallback constant. Anything
/// else quoting it is either a new instance of the #228 bug or a duplicate
/// of the constant that should reuse it instead.
const ALLOWED_FILE: &str = "remind_me_core/src/db/mod.rs";

fn workspace_root() -> PathBuf {
    // crates/remind_me_core -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rust_files_under(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files_under(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_new_dot_remind_me_underscore_literal_appears_outside_the_allowlisted_fallback() {
    let root = workspace_root();
    let mut offenders = Vec::new();

    for crate_dir in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        let src = crate_dir.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rust_files_under(&src, &mut files);
        for file in files {
            // Slash-normalised, workspace-relative, for a stable comparison
            // against `ALLOWED_FILE` regardless of platform path separators.
            let relative = file
                .strip_prefix(root.join("crates"))
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if relative == ALLOWED_FILE {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (i, line) in contents.lines().enumerate() {
                let trimmed = line.trim_start();
                // Doc/line comments may legitimately *discuss* the old
                // directory in prose (e.g. explaining this very fix); only
                // a quoted string literal in real code is the bug this
                // guards against.
                if trimmed.starts_with("//") {
                    continue;
                }
                if line.contains("\".remind_me\"") {
                    offenders.push(format!("{relative}:{}", i + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "found a `.remind_me` (underscore) literal outside the allowlisted \
         fallback in db/mod.rs -- route it through \
         `db::resolve_memory_dir_child` instead: {offenders:?}"
    );
}
