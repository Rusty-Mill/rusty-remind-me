//! Filesystem containment for import and export destinations.
//!
//! # Why the check order is the security property
//!
//! Containment is tested **before** anything touches the filesystem. A check
//! that tested existence first would answer "does this path exist?" for any
//! path on the machine — the tool becomes a filesystem oracle, reporting "file
//! not found" for `/etc/shadow` and "not in roots" for `/etc/passwd`, which is
//! an information leak even though neither is readable.
//!
//! The reference is explicit about this for imports (`SE-02`) and mirrors it
//! for exports. Both sides live here so there is one implementation: two copies
//! of a containment check drift, and the drift is a vulnerability rather than a
//! bug.
//!
//! Paths are resolved before the test, so `..` segments and symlinks cannot
//! step outside a root.

use std::path::{Path, PathBuf};

/// Roots an import may read from, in the platform's own `PATH`-list syntax
/// (`:`-separated on Unix, `;`-separated on Windows — see
/// [`split_path_list`]).
pub const IMPORT_ROOTS_ENV: &str = "REMIND_ME_IMPORT_ROOTS";
/// Roots an export may write to, in the same platform-native syntax as
/// [`IMPORT_ROOTS_ENV`].
pub const EXPORT_ROOTS_ENV: &str = "REMIND_ME_EXPORT_ROOTS";

/// Extensions the importer accepts.
pub const SUPPORTED_SUFFIXES: [&str; 13] = [
    "json", "jsonl", "md", "markdown", "txt", "pdf", "png", "jpg", "jpeg", "mp3", "m4a", "wav",
    "ogg",
];
/// Extensions a *document* import accepts — a document is prose, not a chat log.
pub const DOCUMENT_SUFFIXES: [&str; 3] = ["md", "markdown", "txt"];
/// Extensions OCR'd by an image import.
pub const IMAGE_SUFFIXES: [&str; 3] = ["png", "jpg", "jpeg"];
/// Extensions transcribed by an audio import.
pub const AUDIO_SUFFIXES: [&str; 4] = ["mp3", "m4a", "wav", "ogg"];

/// Windows-portable stand-in for `std::env::var("HOME")`: Unix shells set
/// `$HOME`, Windows does not (it has `%USERPROFILE%` instead), so a bare
/// `std::env::var("HOME")` silently returns `NotPresent` on every Windows
/// machine. `dirs::home_dir()` is already this crate's convention for
/// resolving a home directory everywhere else one is needed
/// (`db::resolve_db_path`, `api_keys::default_store_path`,
/// `remote::default_token_file`) -- this just gives the other home-directory
/// call sites in this module (and their duplicates in `export.rs` and
/// `mempalace_import.rs`) the same portability without changing their
/// `Result<String, VarError>`-shaped call sites at all: every existing
/// `.unwrap()`/`.map(...)`/`.unwrap_or_else(...)` after `std::env::var("HOME")`
/// keeps working unchanged, just pointed at this instead.
pub fn home_dir_var() -> Result<String, std::env::VarError> {
    dirs::home_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or(std::env::VarError::NotPresent)
}

/// Splits a path list the way the platform's own `PATH` is split: `:` on
/// Unix, `;` on Windows.
///
/// A literal `raw.split(':')` — this crate's own convention until this
/// function existed — breaks on Windows, where a colon is not a separator
/// but part of every absolute path's drive letter (`C:\Users\...`).
/// `REMIND_ME_CODE_ROOTS=C:\Users\me\code` would split into `["C",
/// "\Users\me\code"]`, neither of which is the configured root, so nothing
/// ever resolved as contained in it. `std::env::split_paths` is the standard
/// library's own answer to exactly this problem.
pub fn split_path_list(raw: &str) -> Vec<String> {
    std::env::split_paths(raw)
        .filter_map(|p| {
            let s = p.to_string_lossy().trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .collect()
}

fn roots_from(env: &str) -> Vec<PathBuf> {
    match std::env::var(env) {
        Ok(raw) if !raw.trim().is_empty() => split_path_list(&raw)
            .into_iter()
            .map(|r| resolve_lexically(&PathBuf::from(expand_home(&r))))
            .collect(),
        // Default to the home directory, matching the reference. Resolved
        // the same way `resolve_lexically` resolves candidate paths -- on
        // Windows, `canonicalize()` prepends the `\\?\` verbatim-path
        // prefix, so a root left un-resolved would never `starts_with()`
        // match a resolved candidate even when they name the same
        // directory (see this fix's own regression tests).
        _ => home_dir_var()
            .map(|home| vec![resolve_lexically(&PathBuf::from(home))])
            .unwrap_or_default(),
    }
}

/// Roots an import may read from.
pub fn import_roots() -> Vec<PathBuf> {
    roots_from(IMPORT_ROOTS_ENV)
}

/// Roots an export may write to.
pub fn export_roots() -> Vec<PathBuf> {
    roots_from(EXPORT_ROOTS_ENV)
}

pub fn expand_home(raw: &str) -> String {
    match (raw.strip_prefix("~/"), home_dir_var()) {
        (Some(rest), Ok(home)) => format!("{}/{}", home.trim_end_matches('/'), rest),
        _ => raw.to_string(),
    }
}

/// Normalise a path without requiring it to exist.
///
/// Resolves the longest existing prefix through the filesystem — so symlinks
/// are followed — then reattaches the rest with `.` and `..` folded away.
/// `canonicalize` alone fails on a path that does not exist yet, which an
/// export destination usually does not.
pub fn resolve_lexically(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }

    let mut existing = out.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Whether a resolved path sits inside one of `roots`.
pub fn is_contained(resolved: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| resolved == root || resolved.starts_with(root))
}

/// Why an import source was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportPathError {
    OutsideRoots(PathBuf),
    NotFound(PathBuf),
    NotAFile(PathBuf),
    NotADirectory(PathBuf),
    UnsupportedSuffix(String),
}

impl std::fmt::Display for ImportPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoots(p) => {
                write!(f, "Path not in allowed import roots: {}", p.display())
            }
            Self::NotFound(p) => write!(f, "File not found: {}", p.display()),
            Self::NotAFile(p) => write!(f, "Not a file: {}", p.display()),
            Self::NotADirectory(p) => write!(f, "Directory not found: {}", p.display()),
            Self::UnsupportedSuffix(s) => write!(
                f,
                "Unsupported file type: .{}. Use .json, .jsonl, .md, .markdown or .txt",
                s
            ),
        }
    }
}

impl std::error::Error for ImportPathError {}

/// Resolve and validate a file to import.
///
/// Containment first, then existence, then the extension — see the module
/// docs for why that order is not negotiable.
pub fn validate_import_file(raw: &str) -> Result<PathBuf, ImportPathError> {
    let resolved = resolve_lexically(&PathBuf::from(expand_home(raw.trim())));

    if !is_contained(&resolved, &import_roots()) {
        return Err(ImportPathError::OutsideRoots(resolved));
    }
    if !resolved.exists() {
        return Err(ImportPathError::NotFound(resolved));
    }
    if !resolved.is_file() {
        return Err(ImportPathError::NotAFile(resolved));
    }
    let suffix = suffix_of(&resolved);
    if !SUPPORTED_SUFFIXES.contains(&suffix.as_str()) {
        return Err(ImportPathError::UnsupportedSuffix(suffix));
    }
    Ok(resolved)
}

/// Resolve and validate a database file to import from.
///
/// The same containment-then-existence order as [`validate_import_file`], and
/// for the same reason — a caller-supplied path is a caller-supplied path
/// regardless of what is inside it. The extension check is the only thing
/// dropped: a `dbs` archive is a `.sqlite3`/`.db` file, and the importer reads
/// it with SQL rather than by parsing text, so the suffix carries no meaning
/// here. What makes it a database is the file's contents, which the reader
/// finds out when it opens it.
pub fn validate_import_database(raw: &str) -> Result<PathBuf, ImportPathError> {
    let resolved = resolve_lexically(&PathBuf::from(expand_home(raw.trim())));

    if !is_contained(&resolved, &import_roots()) {
        return Err(ImportPathError::OutsideRoots(resolved));
    }
    if !resolved.exists() {
        return Err(ImportPathError::NotFound(resolved));
    }
    if !resolved.is_file() {
        return Err(ImportPathError::NotAFile(resolved));
    }
    Ok(resolved)
}

/// Resolve and validate a directory to import from.
pub fn validate_import_dir(raw: &str) -> Result<PathBuf, ImportPathError> {
    let resolved = resolve_lexically(&PathBuf::from(expand_home(raw.trim())));

    if !is_contained(&resolved, &import_roots()) {
        return Err(ImportPathError::OutsideRoots(resolved));
    }
    if !resolved.is_dir() {
        return Err(ImportPathError::NotADirectory(resolved));
    }
    Ok(resolved)
}

/// Lowercased extension without the dot, or the empty string.
pub fn suffix_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}
