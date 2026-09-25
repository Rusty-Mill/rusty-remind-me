//! Where the database lives (issue #218).
//!
//! The point of these is interoperability, not tidiness. ARCHITECTURE.md
//! Tenet 3 promises a drop-in shared store with `remind_me`, and the schema
//! delivers it — but the two implementations located the file differently, so
//! "drop-in" quietly meant "after you discover two differently-named variables,
//! one taking a directory and one a file".
//!
//! Everything here drives `resolve_db_path_from` rather than `resolve_db_path`.
//! `std::env::set_var` is process-global, and cargo runs a binary's tests on
//! several threads by default: a test that set `REMIND_ME_DB_PATH` would race
//! every other test in this file and pass or fail on scheduling. Injecting the
//! lookup removes the shared mutable state instead of serialising around it.

use remind_me_core::db::{
    resolve_db_path_from, DB_FILE_NAME, DB_PATH_ENV, DEFAULT_DIR_NAME, MCP_DIR_ENV,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from("/home/tester")
}

/// Build an environment lookup from pairs, so each test states exactly what is
/// set and nothing else leaks in.
fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

#[test]
fn nothing_set_matches_the_reference_default() {
    // `remind_me`'s config.py:122 — `~/.remind-me`, then `memory.db`. This is
    // the whole fix in one assertion: unconfigured, both implementations now
    // open the same file. The old default was `remind_me.db` relative to the
    // process's working directory, so it was not even one file.
    let path = resolve_db_path_from(env(&[]), home());
    assert_eq!(path, PathBuf::from("/home/tester/.remind-me/memory.db"));
}

#[test]
fn the_default_directory_uses_a_hyphen_not_an_underscore() {
    // `.remind-me` vs `.remind_me` is a one-character difference that produces
    // a silently separate store, and the port previously used *both* — the
    // runtime path used neither and `configure` wrote the underscore.
    assert_eq!(DEFAULT_DIR_NAME, ".remind-me");
    assert_eq!(DB_FILE_NAME, "memory.db");
}

#[test]
fn the_shared_variable_names_a_directory() {
    // The reference joins `memory.db` onto REMIND_ME_MCP_DIR rather than
    // treating it as a file. Getting this backwards would create a *directory*
    // named memory.db inside a path meant to be the database.
    let path = resolve_db_path_from(env(&[(MCP_DIR_ENV, "/srv/store")]), home());
    assert_eq!(path, PathBuf::from("/srv/store/memory.db"));
}

#[test]
fn the_file_variable_is_used_verbatim() {
    let path = resolve_db_path_from(env(&[(DB_PATH_ENV, "/data/custom.db")]), home());
    assert_eq!(path, PathBuf::from("/data/custom.db"));
}

#[test]
fn the_file_variable_beats_the_directory_variable() {
    // REMIND_ME_DB_PATH is kept ahead of the shared variable rather than
    // dropped, because every MCP client entry `configure` has ever written sets
    // it. Reversing this would silently relocate those installs.
    let path = resolve_db_path_from(
        env(&[
            (DB_PATH_ENV, "/data/custom.db"),
            (MCP_DIR_ENV, "/srv/store"),
        ]),
        home(),
    );
    assert_eq!(path, PathBuf::from("/data/custom.db"));
}

// ---------------------------------------------------------------------------
// Blank is unset
// ---------------------------------------------------------------------------

#[test]
fn a_blank_variable_falls_through_rather_than_resolving_to_nothing() {
    // A blank env var is how "unset" arrives from a lot of process managers.
    // Without this, `REMIND_ME_DB_PATH=""` opens a database at the empty path,
    // which is a confusing failure rather than the default.
    for blank in ["", "   ", "\t"] {
        let path = resolve_db_path_from(env(&[(DB_PATH_ENV, blank)]), home());
        assert_eq!(
            path,
            PathBuf::from("/home/tester/.remind-me/memory.db"),
            "blank {blank:?} should fall through to the default"
        );
    }
}

#[test]
fn a_blank_directory_variable_falls_through_to_the_default_not_to_memory_db() {
    // The sharp version of the case above: joining `memory.db` onto a blank
    // directory yields the relative path `memory.db`, which opens *something*
    // in the working directory and looks like an empty vault.
    let path = resolve_db_path_from(env(&[(MCP_DIR_ENV, "  ")]), home());
    assert_eq!(path, PathBuf::from("/home/tester/.remind-me/memory.db"));
}

// ---------------------------------------------------------------------------
// Tilde expansion
// ---------------------------------------------------------------------------

#[test]
fn a_leading_tilde_expands_in_both_variables() {
    // The reference calls `.expanduser()`. This matters here because
    // `configure` writes these variables into MCP client *JSON*, where no shell
    // ever expands them — a literal `~` would create a directory named `~`.
    assert_eq!(
        resolve_db_path_from(env(&[(DB_PATH_ENV, "~/vault/mem.db")]), home()),
        PathBuf::from("/home/tester/vault/mem.db")
    );
    assert_eq!(
        resolve_db_path_from(env(&[(MCP_DIR_ENV, "~/vault")]), home()),
        PathBuf::from("/home/tester/vault/memory.db")
    );
}

#[test]
fn a_bare_tilde_is_the_home_directory() {
    assert_eq!(
        resolve_db_path_from(env(&[(MCP_DIR_ENV, "~")]), home()),
        PathBuf::from("/home/tester/memory.db")
    );
}

#[test]
fn another_users_tilde_is_left_alone() {
    // `~alice` needs the password database to resolve. Treating it as
    // `$HOME/alice` would point at a path that plausibly exists and is wrong,
    // which is worse than leaving it literal.
    let path = resolve_db_path_from(env(&[(DB_PATH_ENV, "~alice/mem.db")]), home());
    assert_eq!(path, PathBuf::from("~alice/mem.db"));
}

#[test]
fn a_tilde_inside_a_path_is_not_expanded() {
    // Only a *leading* tilde is a home reference; `/srv/~backup` is a directory
    // someone actually named that.
    let path = resolve_db_path_from(env(&[(DB_PATH_ENV, "/srv/~backup/mem.db")]), home());
    assert_eq!(path, PathBuf::from("/srv/~backup/mem.db"));
}

// ---------------------------------------------------------------------------
// The interop claim itself
// ---------------------------------------------------------------------------

#[test]
fn one_setting_aims_both_implementations_at_one_file() {
    // This is the assertion the issue was filed for. `remind_me` computes
    // `Path(REMIND_ME_MCP_DIR).expanduser() / "memory.db"`; reproduced here as
    // the expected value so a change to our resolution has to restate the
    // reference's rule rather than quietly drift from it.
    let shared_dir = "/shared/remind-me";
    let reference_would_open = PathBuf::from(shared_dir).join("memory.db");

    let we_open = resolve_db_path_from(env(&[(MCP_DIR_ENV, shared_dir)]), home());

    assert_eq!(
        we_open, reference_would_open,
        "REMIND_ME_MCP_DIR must aim both implementations at the same file"
    );
}
