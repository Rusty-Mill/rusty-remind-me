pub mod migrations;
pub mod queries;
pub mod schema;

use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, Result};
use std::path::{Path, PathBuf};

/// Points directly at a database *file*. This crate's own variable.
pub const DB_PATH_ENV: &str = "REMIND_ME_DB_PATH";

/// Names the *directory* holding `memory.db`. This is `remind_me`'s variable
/// (`config.py:122`), honoured here so one setting aims both implementations
/// at one file.
pub const MCP_DIR_ENV: &str = "REMIND_ME_MCP_DIR";

/// The filename inside [`MCP_DIR_ENV`]. Fixed by the reference, not a choice.
pub const DB_FILE_NAME: &str = "memory.db";

/// The directory used when neither variable is set — `~/.remind-me`, matching
/// the reference's default. Hyphen, not underscore.
pub const DEFAULT_DIR_NAME: &str = ".remind-me";

/// The pre-#228 default directory, before the hyphen fix landed. Read-only:
/// [`resolve_memory_dir_child`]'s fallback for a file/directory a user
/// already has here, never a location anything in this crate writes to.
const LEGACY_UNDERSCORE_DIR_NAME: &str = ".remind_me";

/// Where the database lives, given the environment.
///
/// # Why this honours a variable belonging to another implementation
///
/// ARCHITECTURE.md Tenet 3 promises drop-in interoperability with `remind_me`,
/// and the schema delivers it — both sides read and write each other's rows in
/// one v29 file. Locating that file was the part that did not work.
///
/// Before this, the port read only `REMIND_ME_DB_PATH`, which appears nowhere
/// in the reference, and defaulted to `remind_me.db` *relative to the current
/// working directory*. The reference reads only `REMIND_ME_MCP_DIR` and
/// defaults to `~/.remind-me/memory.db`. Unconfigured, the two never opened the
/// same file; configured with the variable a port user knows, the reference
/// silently ignored it and kept using its own default. Both commands succeeded
/// and printed sensible output while operating on different databases — which
/// is exactly how a test write ended up in a real memory store (#218).
///
/// # Precedence
///
/// 1. `REMIND_ME_DB_PATH` — a file path, so it is the most specific and wins.
///    Kept ahead of the shared variable rather than dropped: existing callers
///    set it, including every MCP client `configure` has ever written.
/// 2. `$REMIND_ME_MCP_DIR/memory.db` — the shared setting.
/// 3. `~/.remind-me/memory.db` — the reference's default.
///
/// A variable set to the empty string is treated as unset, which is how "unset"
/// arrives from a lot of process managers.
pub fn resolve_db_path() -> PathBuf {
    resolve_db_path_from(
        |name| std::env::var(name).ok(),
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// [`resolve_db_path`] with the environment and home directory injected.
///
/// Split out so the precedence can be tested without `set_var`, which is
/// process-global and races every other test in the binary.
pub fn resolve_db_path_from<F>(get: F, home: PathBuf) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    let non_empty = |name: &str| get(name).filter(|v| !v.trim().is_empty());

    if let Some(path) = non_empty(DB_PATH_ENV) {
        return expand_tilde(&path, &home);
    }
    resolve_memory_dir_from(&get, home).join(DB_FILE_NAME)
}

/// Where per-user files other than the database live: `$REMIND_ME_MCP_DIR`
/// or `~/.remind-me` — the directory half of [`resolve_db_path`]'s
/// precedence, lifted out (as suggested in #228) so every other per-user
/// file this crate writes (wiki root, ICS feed token, API key store,
/// connector token, OAuth state) resolves under the same directory as the
/// database instead of drifting to its own ad-hoc default.
///
/// No `REMIND_ME_DB_PATH`-equivalent override here: that variable names a
/// *file*, and there is no directory-shaped analogue to prefer ahead of
/// `MCP_DIR_ENV`. Each caller keeps its own explicit override env var
/// (`REMIND_ME_WIKI_DIR`, `REMIND_ME_ICS_TOKEN_FILE`, ...) for that, checked
/// before ever calling this.
pub fn resolve_memory_dir() -> PathBuf {
    resolve_memory_dir_from(
        |name| std::env::var(name).ok(),
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// [`resolve_memory_dir`] with the environment and home directory injected —
/// same reason as [`resolve_db_path_from`].
pub fn resolve_memory_dir_from<F>(get: F, home: PathBuf) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    match get(MCP_DIR_ENV).filter(|v| !v.trim().is_empty()) {
        Some(dir) => expand_tilde(&dir, &home),
        None => home.join(DEFAULT_DIR_NAME),
    }
}

/// [`resolve_memory_dir`] plus a filename, with one safety net for the #228
/// rename (`~/.remind_me` → `~/.remind-me`, underscore to hyphen): when
/// `REMIND_ME_MCP_DIR` is unset (so the *default* directory applies) and
/// `name` does not exist under the new default but does exist under the
/// pre-fix underscored one, that legacy path is returned instead — so a
/// wiki page, API key store, calendar token, or connector credential nobody
/// re-created under the new directory is found rather than silently
/// orphaned. Only ever read from here, never written to or migrated: the
/// old directory is left exactly as it is.
///
/// `REMIND_ME_MCP_DIR` being set opts out of the fallback entirely — an
/// explicitly chosen directory has no "legacy" counterpart to fall back to.
pub fn resolve_memory_dir_child(name: &str) -> PathBuf {
    resolve_memory_dir_child_from(
        |key| std::env::var(key).ok(),
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
        name,
    )
}

/// [`resolve_memory_dir_child`] with the environment and home directory
/// injected — same reason as [`resolve_db_path_from`].
pub fn resolve_memory_dir_child_from<F>(get: F, home: PathBuf, name: &str) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    let mcp_dir_overridden = get(MCP_DIR_ENV).is_some_and(|v| !v.trim().is_empty());
    let new_path = resolve_memory_dir_from(&get, home.clone()).join(name);
    if mcp_dir_overridden || new_path.exists() {
        return new_path;
    }
    let legacy_path = home.join(LEGACY_UNDERSCORE_DIR_NAME).join(name);
    if legacy_path.exists() {
        return legacy_path;
    }
    new_path
}

/// Expand a leading `~` against `home`, as the reference's `expanduser` does.
///
/// Not cosmetic. `configure` writes these variables into MCP client JSON, where
/// no shell is involved — a literal `~/.remind-me` would become a directory
/// named `~` beside the client's working directory, and the resulting database
/// would look empty rather than misplaced.
///
/// Only a leading `~` or `~/` is expanded. `~user` is deliberately left alone:
/// resolving another user's home needs the password database, and silently
/// treating `~alice/db` as a relative path is less wrong than guessing.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    match raw.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(raw),
    }
}

pub struct Database {
    conn: Mutex<Connection>,
    /// `None` for an in-memory database. Kept so [`Database::open_secondary`]
    /// can reopen the same on-disk file without every caller having to carry
    /// the path around separately.
    path: Option<PathBuf>,
}

impl Database {
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        schema::initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path),
        })
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Opens a second, independent connection to the same on-disk file this
    /// `Database` wraps — for callers (namely [`crate::sync::SyncWorker`])
    /// that must not hold the process-wide [`Database::conn`] `Mutex` across
    /// long-running work such as network I/O. Every other MCP tool call
    /// needs that mutex; a sync cycle pushing/pulling several remotes can run
    /// for many multiples of any one HTTP timeout, and previously held the
    /// whole process's database hostage for that entire span. WAL mode plus
    /// the `busy_timeout` `schema::initialize_schema` sets let this
    /// connection write concurrently with the primary one; SQLite's own
    /// briefly-held, per-statement file lock replaces the Rust-level lock
    /// for the moments a caller like that actually needs it.
    ///
    /// `Err` for an in-memory database: a second `:memory:` connection would
    /// open a distinct, disconnected store, not a second handle onto the
    /// same data.
    pub fn open_secondary(&self) -> std::result::Result<Connection, String> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| "in-memory database has no file to reopen".to_string())?;
        Self::open_secondary_at(path)
    }

    /// The same connection [`Database::open_secondary`] opens (WAL mode plus
    /// the `busy_timeout` `schema::initialize_schema` sets), for a caller that
    /// only has a path and not an existing `Database`/`Arc` to call it
    /// through — namely a background thread that reopens its own connection
    /// on every retry, the same shape [`crate::scheduler::start_scheduler`]/
    /// [`crate::watcher::start_watcher`] already use for *their* threads.
    /// Those two use a bare `Connection::open` with no pragma setup, which is
    /// fine for their short, local-only writes; [`crate::sync::SyncWorker`]'s
    /// writes can share a transaction with a network round-trip, so it keeps
    /// needing the pragmas a plain `Connection::open` would silently skip.
    pub fn open_secondary_at<P: AsRef<Path>>(path: P) -> std::result::Result<Connection, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        schema::initialize_schema(&conn).map_err(|e| e.to_string())?;
        Ok(conn)
    }
}
