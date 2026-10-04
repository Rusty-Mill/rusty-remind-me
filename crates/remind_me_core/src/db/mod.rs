//! The node's store: the engine tables (ADR-0023, ADR-0025) behind one
//! [`Database`], and the repositories every caller reads and writes through.
//!
//! The database is still named by the path of the old SQLite `memory.db`:
//! the engine directory stands beside it (`memory.engine`), and a file with
//! no directory yet is copied onto the engine on the first open
//! (`db::legacy_sqlite`, `db::engine::copy`), then never opened again.

pub mod archives;
pub mod curation;
pub mod derived;
pub mod engine;
pub mod entities;
pub mod feedback;
pub mod history;
pub mod imports;
pub mod legacy_sqlite;
pub mod memories;
pub mod outbox;
pub mod promotions;
pub mod queries;
pub mod references;
pub mod related;
pub mod reminders;
pub mod saved_searches;
pub mod sessions;
pub mod stats;
pub mod store;
pub mod sync_feed;
pub mod sync_state;
pub mod vectors;
pub mod wiki;

pub use legacy_sqlite::SCHEMA_VERSION;
pub use store::{Result, SecondarySource, Store, StoreError};

use engine::{EngineHandle, EngineLock, EngineTables};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// The engine directory that holds the store of the database at `path`:
/// `memory.db` has `memory.engine`. The file itself is the old SQLite store,
/// copied onto the engine on the first open and never opened again.
pub fn engine_dir(path: &Path) -> PathBuf {
    path.with_extension("engine")
}

/// Where a copy into the engine directory `dir` is built before it is
/// renamed to `dir`: `memory.engine` has `memory.engine.partial`. While it
/// exists without `dir`, a copy is under way, or one failed and was left
/// for inspection.
pub fn partial_dir(dir: &Path) -> PathBuf {
    let mut partial = dir.as_os_str().to_owned();
    partial.push(".partial");
    PathBuf::from(partial)
}

/// Copy the SQLite file `file` into the engine directory `dir` on a store's
/// first open, saying on stderr what it is doing. The copy runs once and
/// takes minutes on a large store (two for a real 15,000-memory node), so a
/// node that went quiet for that long would look hung. A daemon's stderr is
/// its `.daemon.log`.
fn copy_logging_progress(file: &Path, dir: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    eprintln!(
        "rusty-remind-me: copying {} onto the engine at {}. This happens once, on the \
         first start with this build, and can take a few minutes on a large store; \
         clients wait for it.",
        file.display(),
        dir.display()
    );
    let mut log_table = |done: engine::copy::TableDone| {
        if done.rows > 0 {
            eprintln!(
                "rusty-remind-me: copied {} rows of {} in {:.1}s",
                done.rows,
                done.table,
                done.elapsed.as_secs_f64()
            );
        }
    };
    engine::copy::copy_into_place(file, dir, &mut log_table)?;
    eprintln!(
        "rusty-remind-me: copy finished in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The node's store: the engine tables, their process-wide lock, and the
/// file they stand beside.
pub struct Database {
    /// Taken by [`Database::store`], so one caller runs at a time, as the
    /// SQLite connection's mutex once ordered them. Background threads use
    /// a [`SecondarySource`] and take no part in it.
    lock: Mutex<()>,
    engine: EngineHandle,
    /// `None` for an in-memory database. Kept so [`Database::secondary_source`]
    /// can name the file without every caller carrying the path around.
    path: Option<PathBuf>,
}

impl Database {
    /// A database that lives only as long as this value: temporary engine
    /// tables, removed when it is dropped.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_tables(EngineTables::open_temporary()?, None)
    }

    /// The database named by `path`, the old SQLite file's path: its store
    /// is the engine directory beside it (see [`engine_dir`]), created on
    /// the first open. A SQLite file there with no engine directory yet is
    /// copied onto the engine first, verified, and left as it was.
    ///
    /// `:memory:`, SQLite's name for a database with no file, opens an
    /// in-memory database.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if another process holds the directory (the
    /// store daemon, usually) or a table cannot be opened;
    /// [`StoreError::Invalid`] when the copy refused a row or the file is at
    /// a schema version the copy does not read, leaving the file as it was.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if path == Path::new(":memory:") {
            return Self::open_in_memory();
        }
        let dir = engine_dir(path);
        let copied = !dir.exists() && path.is_file();
        if copied {
            copy_logging_progress(path, &dir)?;
        }
        let db = Self::from_tables(EngineTables::open(&dir)?, Some(path.to_path_buf()))?;
        if copied {
            // The SQLite store's open used to rewrite entity ids an earlier
            // build derived differently; a copied store gets that once.
            crate::entity::renormalize_entity_ids(&db.store())?;
        }
        Ok(db)
    }

    fn from_tables(tables: EngineTables, path: Option<PathBuf>) -> Result<Self> {
        let db = Self {
            lock: Mutex::new(()),
            engine: Arc::new(EngineLock::new(tables)),
            path,
        };
        // Every outbox write is gated on the `sync_enabled` flag: align it
        // with this process's configuration before anything touches the
        // outbox, as every open always has.
        crate::sync::reconcile_sync_enabled_flag(&db.store())?;
        // Nothing here drains the outbox; the retention rule keeps it from
        // growing without bound.
        crate::sync::prune_outbox(&db.store())?;
        // A changed embedding model invalidates every stored vector (#96).
        if let Some(embedder) = crate::embedder::resolve_embedder() {
            crate::vectors::reconcile_embedding_meta(&db.store(), &embedder.identity())?;
        }
        queries::empty_tombstones(&db.store())?;
        Ok(db)
    }

    /// Compact the engine tables that have something to reclaim (see
    /// [`EngineTables::compact_needed`]). How many were compacted.
    pub fn compact_store(&self) -> Result<usize> {
        self.engine.lock().compact_needed()
    }

    /// The store, holding the database's lock for as long as the handle
    /// lives.
    pub fn store(&self) -> Store<'_> {
        Store::locked(self.lock.lock(), self.engine.clone(), self.path.clone())
    }

    /// Where a background thread reaches this database's tables without
    /// its lock. `None` for an in-memory database.
    pub fn secondary_source(&self) -> Option<SecondarySource> {
        Some(SecondarySource::new(
            self.path.clone()?,
            self.engine.clone(),
        ))
    }

    /// The shared engine tables, for a test that builds its own
    /// [`SecondarySource`].
    #[cfg(test)]
    pub(crate) fn engine_handle(&self) -> EngineHandle {
        self.engine.clone()
    }
}
