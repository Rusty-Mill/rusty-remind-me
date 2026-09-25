//! Coverage for `import_paths::split_path_list` (#298).
//!
//! `CODE_ROOTS_ENV`/`IMPORT_ROOTS_ENV`/`EXPORT_ROOTS_ENV`/`WATCH_DIRS_ENV`
//! all used to split on a literal `':'`, a Unix `PATH`-style convention that
//! breaks on Windows: an absolute Windows path's drive letter is itself a
//! colon (`C:\Users\...`), so a single configured root split into two
//! bogus, non-matching fragments. `split_path_list` uses
//! `std::env::split_paths` instead, which is platform-aware by
//! construction -- `:` on Unix, `;` on Windows.
//!
//! The Windows-specific case can only be compiled and actually exercised on
//! Windows, since `std::env::split_paths`'s behavior is a `cfg`-gated
//! compile-time choice, not a runtime one this test could fake on Linux.
//! That is exactly what the `windows` CI job added for #271 now runs.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::import_paths::{
    is_contained, resolve_lexically, split_path_list, validate_import_file, ImportPathError,
    IMPORT_ROOTS_ENV,
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[test]
fn a_single_path_is_one_entry() {
    assert_eq!(split_path_list("/home/me/notes"), vec!["/home/me/notes"]);
}

#[test]
fn empty_and_whitespace_only_segments_are_dropped() {
    assert_eq!(
        split_path_list(""),
        Vec::<String>::new(),
        "an empty string has no path segments at all"
    );
}

#[cfg(not(windows))]
#[test]
fn multiple_unix_paths_split_on_colon() {
    assert_eq!(
        split_path_list("/a/b:/c/d"),
        vec!["/a/b".to_string(), "/c/d".to_string()]
    );
}

#[cfg(not(windows))]
#[test]
fn surrounding_whitespace_is_trimmed() {
    assert_eq!(split_path_list("  /a/b  "), vec!["/a/b".to_string()]);
}

/// The regression this whole function exists to fix: a single Windows root
/// must survive as one entry, not be split at its own drive letter's colon.
/// Only meaningful -- and only compiled -- on Windows, since
/// `std::env::split_paths` is `:`-splitting by construction everywhere
/// else; a `cfg(windows)`-gated assertion is the only honest way to prove
/// this without faking platform behavior on Linux.
#[cfg(windows)]
#[test]
fn a_windows_drive_letter_is_not_mistaken_for_a_separator() {
    assert_eq!(
        split_path_list(r"C:\Users\me\code"),
        vec![r"C:\Users\me\code".to_string()]
    );
}

#[cfg(windows)]
#[test]
fn multiple_windows_paths_split_on_semicolon() {
    assert_eq!(
        split_path_list(r"C:\Users\me\code;D:\notes"),
        vec![r"C:\Users\me\code".to_string(), r"D:\notes".to_string()]
    );
}

// ---------------------------------------------------------------------------
// `is_contained` (#284) -- this crate's tests used to only exercise this
// through `remind_me_api`'s HTTP-level import/export tests, giving
// `cargo test -p remind_me_core` zero signal on the security-critical check
// itself. These are pure-logic tests: no filesystem, no env vars.
// ---------------------------------------------------------------------------

#[test]
fn a_root_itself_and_anything_under_it_is_contained() {
    let root = PathBuf::from("/roots/a");
    let roots = vec![root.clone()];
    assert!(is_contained(&root, &roots), "the root itself must count");
    assert!(is_contained(&root.join("file.md"), &roots));
    assert!(is_contained(&root.join("sub/deep/file.md"), &roots));
}

#[test]
fn a_path_outside_every_configured_root_is_not_contained() {
    let roots = vec![PathBuf::from("/roots/a"), PathBuf::from("/roots/b")];
    assert!(!is_contained(&PathBuf::from("/etc/passwd"), &roots));
}

#[test]
fn a_textual_sibling_prefix_is_not_contained() {
    // `/roots/a-evil` starts with the *bytes* of `/roots/a`, but
    // `Path::starts_with` compares components, not bytes -- a root of
    // `/roots/a` must not accidentally authorise `/roots/a-evil`.
    let roots = vec![PathBuf::from("/roots/a")];
    assert!(!is_contained(&PathBuf::from("/roots/a-evil/file"), &roots));
}

#[test]
fn containment_is_checked_against_every_configured_root() {
    let roots = vec![PathBuf::from("/roots/a"), PathBuf::from("/roots/b")];
    assert!(is_contained(&PathBuf::from("/roots/b/notes.md"), &roots));
}

#[test]
fn an_empty_roots_list_contains_nothing() {
    assert!(!is_contained(&PathBuf::from("/anything"), &[]));
}

// ---------------------------------------------------------------------------
// `resolve_lexically`
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
#[test]
fn parent_dir_components_are_folded_without_touching_the_filesystem() {
    // `/a` does not exist on this machine, so this also proves the folding
    // happens lexically -- `..` is resolved by popping the in-memory
    // component list, not by asking the filesystem what the parent is.
    assert_eq!(
        resolve_lexically(Path::new("/a/b/../c/./d")),
        PathBuf::from("/a/c/d")
    );
}

// Windows counterpart of the test above: same lexical-folding behavior, but
// `resolve_lexically` returns a `\\?\`-prefixed, backslash-separated path on
// this platform (see its own doc comment), so the expected value has to
// match that shape rather than the Unix one. An explicit drive letter keeps
// this deterministic regardless of which drive the CI runner's checkout
// happens to be on.
#[cfg(windows)]
#[test]
fn parent_dir_components_are_folded_without_touching_the_filesystem() {
    assert_eq!(
        resolve_lexically(Path::new(r"C:\a\b\..\c\.\d")),
        PathBuf::from(r"\\?\C:\a\c\d")
    );
}

#[test]
fn a_relative_path_is_anchored_to_the_current_directory() {
    let resolved = resolve_lexically(Path::new("some/nonexistent/relative/path.md"));
    assert!(
        resolved.is_absolute(),
        "relative input must resolve absolute"
    );
    assert!(
        resolved.ends_with("some/nonexistent/relative/path.md"),
        "resolved path lost its relative tail: {}",
        resolved.display()
    );
}

#[cfg(not(windows))]
#[test]
fn a_nonexistent_path_still_resolves_rather_than_failing() {
    // Unlike `Path::canonicalize`, which errors on a path that does not
    // exist -- an export destination usually does not yet.
    let resolved = resolve_lexically(Path::new("/definitely/not/on/this/machine-284.md"));
    assert_eq!(
        resolved,
        PathBuf::from("/definitely/not/on/this/machine-284.md")
    );
}

// Windows counterpart: same "resolves without touching the filesystem"
// guarantee, but the expected shape is `\\?\`-prefixed and backslash
// separated on this platform, matching `resolve_lexically`'s documented
// Windows behavior. An explicit drive letter keeps this deterministic
// regardless of which drive the CI runner's checkout happens to be on.
#[cfg(windows)]
#[test]
fn a_nonexistent_path_still_resolves_rather_than_failing() {
    let resolved = resolve_lexically(Path::new(r"C:\definitely\not\on\this\machine-284.md"));
    assert_eq!(
        resolved,
        PathBuf::from(r"\\?\C:\definitely\not\on\this\machine-284.md")
    );
}

// ---------------------------------------------------------------------------
// `validate_import_file`, against an explicit, isolated root (rather than
// the default home-directory root other tests in this crate rely on) --
// direct coverage of the module the security property lives in, one level
// below the importer entry points `importer_test.rs` already covers.
// ---------------------------------------------------------------------------

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A scratch import root, isolated per test, with `IMPORT_ROOTS_ENV` pointed
/// at it for the guard's lifetime. Callers must hold `env_lock` first.
struct Root {
    dir: PathBuf,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_import_paths_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let dir: PathBuf = dir.to_string_lossy().replace(['(', ')', ' '], "").into();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::test_env::set_var(IMPORT_ROOTS_ENV, &dir);
        Self { dir }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        crate::test_env::remove_var(IMPORT_ROOTS_ENV);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_path_outside_the_root_is_rejected_whether_or_not_it_exists() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _root = Root::new("oracle");

    // Containment runs before the existence check, so a real file and an
    // imaginary one outside the root must fail identically -- otherwise the
    // importer becomes a filesystem oracle, answering "does this exist?"
    // for any path on the machine.
    assert!(matches!(
        validate_import_file("/etc/passwd"),
        Err(ImportPathError::OutsideRoots(_))
    ));
    assert!(matches!(
        validate_import_file("/etc/definitely-not-here-284-xyz"),
        Err(ImportPathError::OutsideRoots(_))
    ));
}

#[test]
fn a_traversal_out_of_an_explicit_root_is_rejected() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("traversal");

    let raw = format!("{}/../../etc/passwd", root.dir.display());
    assert!(matches!(
        validate_import_file(&raw),
        Err(ImportPathError::OutsideRoots(_))
    ));
}

#[test]
#[cfg(unix)]
fn a_symlink_inside_the_root_pointing_outside_it_does_not_grant_access() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("symlink-root");

    let outside = std::env::temp_dir().join(format!(
        "rrm_import_paths_outside_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let outside: PathBuf = outside
        .to_string_lossy()
        .replace(['(', ')', ' '], "")
        .into();
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "top secret").unwrap();

    std::os::unix::fs::symlink(&outside, root.dir.join("escape")).unwrap();

    // `resolve_lexically` follows the symlink through the filesystem before
    // the containment test runs, so the resolved path lands outside the
    // root even though the raw string looks like it is inside it.
    let raw = root.dir.join("escape/secret.md").display().to_string();
    let result = validate_import_file(&raw);

    assert!(
        matches!(result, Err(ImportPathError::OutsideRoots(_))),
        "a symlink inside the root pointed outside it and was still accepted: {result:?}"
    );

    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_missing_file_inside_the_root_reports_not_found() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("missing");

    let raw = root.dir.join("nope.md").display().to_string();
    assert!(matches!(
        validate_import_file(&raw),
        Err(ImportPathError::NotFound(_))
    ));
}

#[test]
fn a_directory_is_rejected_as_not_a_file() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("isdir");
    let sub = root.dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    assert!(matches!(
        validate_import_file(&sub.display().to_string()),
        Err(ImportPathError::NotAFile(_))
    ));
}

#[test]
fn an_unsupported_extension_is_rejected() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("suffix");
    let path = root.dir.join("installer.exe");
    std::fs::write(&path, "not really an executable").unwrap();

    assert!(matches!(
        validate_import_file(&path.display().to_string()),
        Err(ImportPathError::UnsupportedSuffix(_))
    ));
}

#[test]
fn a_supported_file_inside_the_root_resolves_successfully() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let root = Root::new("ok");
    let path = root.dir.join("notes.md");
    std::fs::write(&path, "# Notes\n\nsomething worth keeping").unwrap();

    let resolved = validate_import_file(&path.display().to_string()).unwrap();
    assert!(resolved.ends_with("notes.md"));
}
