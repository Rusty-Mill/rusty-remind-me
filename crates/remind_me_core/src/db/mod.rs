pub mod archives;
pub mod curation;
pub mod derived;
#[cfg(feature = "engine-store")]
pub mod engine;
pub mod entities;
pub mod feedback;
pub mod history;
pub mod imports;
pub mod memories;
pub mod migrations;
pub mod outbox;
pub mod promotions;
pub mod queries;
pub mod related;
pub mod reminders;
pub mod saved_searches;
pub mod schema;
pub mod stats;
pub mod store;
pub mod sync_feed;
pub mod sync_state;
pub mod vectors;
pub mod wiki;

pub use store::{Result, SecondarySource, Store, StoreError};

use parking_lot::Mutex;
use rusqlite::Connection;
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

/// The file `conn` has open, or `None` for an in-memory database.
pub fn database_path(store: &Store<'_>) -> Result<Option<PathBuf>> {
    let conn = store.conn();
    let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

/// The engine directory that holds the store of the SQLite file at `path`
/// once it has been copied there: `memory.db` has `memory.engine`.
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
/// first open, saying on stderr what it is doing. The copy runs once, on the
/// first start with an engine build, and takes minutes on a large store (two
/// for a real 15,000-memory node), so a node that went quiet for that long
/// would look hung. A daemon's stderr is its `.daemon.log`.
#[cfg(feature = "engine-store")]
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

pub struct Database {
    conn: Mutex<Connection>,
    /// The engine tables for the groups moved so far (ADR-0023, phase 4),
    /// when this database was opened on the engine.
    #[cfg(feature = "engine-store")]
    engine: Option<engine::EngineHandle>,
    /// `None` for an in-memory database. Kept so [`Database::open_secondary`]
    /// can reopen the same on-disk file without every caller having to carry
    /// the path around separately.
    path: Option<PathBuf>,
}

impl Database {
    /// A database that lives only as long as this value.
    ///
    /// On the engine (the default with the `engine-store` feature), the
    /// store lives in temporary engine tables; with `REMIND_ME_STORE=sqlite`,
    /// or in a build without the feature, it is in-memory SQLite. That
    /// variable is how the test suite runs against both backends.
    pub fn open_in_memory() -> Result<Self> {
        #[cfg(feature = "engine-store")]
        if engine::engine_selected() {
            return Self::open_in_memory_on_engine();
        }
        Self::open_sqlite_in_memory()
    }

    /// An in-memory database with the moved groups on temporary engine
    /// tables, whatever `REMIND_ME_STORE` says.
    #[cfg(feature = "engine-store")]
    pub fn open_in_memory_on_engine() -> Result<Self> {
        let mut db = Self::open_sqlite_in_memory()?;
        let tables = engine::EngineTables::open_temporary()?;
        db.engine = Some(std::sync::Arc::new(engine::EngineLock::new(tables)));
        // Opening the schema aligned the gate in SQLite's `sync_flags`; the
        // engine holds its own `sync_flags` now, so align that one too, as
        // every open does, before anything touches the outbox.
        crate::sync::reconcile_sync_enabled_flag(&db.store())?;
        queries::empty_tombstones(&db.store())?;
        Ok(db)
    }

    /// An in-memory SQLite database whatever `REMIND_ME_STORE` says: for
    /// tests of what only SQLite has, such as the schema and its migrations.
    pub fn open_sqlite_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            #[cfg(feature = "engine-store")]
            engine: None,
            path: None,
        })
    }

    /// The database in the SQLite file at `path`.
    ///
    /// On the engine (the default with the `engine-store` feature, unless
    /// `REMIND_ME_STORE=sqlite`), the store lives in the engine directory
    /// beside the file (see [`engine_dir`]), copied from the file on the
    /// first such open. A file
    /// whose store has moved there is refused without the engine: its rows
    /// stopped changing at the copy.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        #[cfg(feature = "engine-store")]
        if engine::engine_selected() {
            // SQLite's name for a database with no file: there is nothing
            // to copy, and nowhere beside it to keep engine tables.
            if path == Path::new(":memory:") {
                return Self::open_in_memory_on_engine();
            }
            return Self::open_on_engine(path);
        }
        Self::open_on_sqlite(path)
    }

    /// The database in the SQLite file at `path`, whatever
    /// `REMIND_ME_STORE` says.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] when the file's store was copied onto the
    /// engine: its rows stopped changing at the copy.
    pub fn open_on_sqlite<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let dir = engine_dir(path);
        if dir.exists() {
            return Err(StoreError::Invalid(format!(
                "the store in {} was copied onto the engine in {}, and the file has not \
                 changed since. Open it on the engine (a build with the engine store, \
                 without REMIND_ME_STORE=sqlite); to go back to SQLite as of the copy, \
                 move {} aside first",
                path.display(),
                dir.display(),
                dir.display()
            )));
        }
        let db = Self::open_sqlite_file(path)?;
        // Only when SQLite is the store: on the engine the file stopped
        // changing at the copy, and the engine open empties its own.
        queries::empty_tombstones(&db.store())?;
        Ok(db)
    }

    /// The database at `path` with its store on the engine, whatever
    /// `REMIND_ME_STORE` says. The first open copies the SQLite file into
    /// the engine directory; later opens use that directory as it is.
    ///
    /// # Errors
    ///
    /// Besides the errors of opening either store: [`StoreError::Invalid`]
    /// when the copy refused a row, leaving the SQLite file as it was.
    #[cfg(feature = "engine-store")]
    pub fn open_on_engine<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let mut db = Self::open_sqlite_file(path)?;
        let dir = engine_dir(path);
        db.copy_on_first_open(&dir)?;
        let tables = engine::EngineTables::open(&dir)?;
        db.engine = Some(std::sync::Arc::new(engine::EngineLock::new(tables)));
        crate::sync::reconcile_sync_enabled_flag(&db.store())?;
        queries::empty_tombstones(&db.store())?;
        Ok(db)
    }

    /// Copy the SQLite file into `dir` unless that exists already.
    ///
    /// Holds SQLite's write lock across the copy, so no other process
    /// writes a row after the copy read its table, and a second process
    /// opening the same file waits and then finds the copy done.
    #[cfg(feature = "engine-store")]
    fn copy_on_first_open(&self, dir: &Path) -> Result<()> {
        if dir.exists() {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let copied = match self.path.as_deref() {
            Some(file) if !dir.exists() => copy_logging_progress(file, dir),
            _ => Ok(()),
        };
        conn.execute_batch("ROLLBACK")?;
        copied
    }

    /// The SQLite file at `path`, migrated to this build's schema.
    fn open_sqlite_file(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        schema::initialize_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            #[cfg(feature = "engine-store")]
            engine: None,
            path: Some(path.to_path_buf()),
        })
    }

    /// Compact the engine tables that have something to reclaim (see
    /// [`engine::EngineTables::compact_needed`]). How many were compacted;
    /// always 0 on SQLite, which reclaims space on its own terms.
    pub fn compact_store(&self) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = &self.engine {
            return engine.lock().compact_needed();
        }
        Ok(0)
    }

    /// The store, locked for as long as the handle lives.
    pub fn store(&self) -> Store<'_> {
        let store = Store::locked(self.conn.lock());
        #[cfg(feature = "engine-store")]
        let store = store.with_engine(self.engine.clone());
        store
    }

    /// Where a background thread reopens this database: its file and its
    /// engine tables. `None` for an in-memory database.
    pub fn secondary_source(&self) -> Option<SecondarySource> {
        Some(SecondarySource::new(
            self.path.clone()?,
            #[cfg(feature = "engine-store")]
            self.engine.clone(),
        ))
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

/// Run `test` against every backend this build has: SQLite always, the
/// engine too with `engine-store`.
#[cfg(test)]
pub(crate) fn on_each_backend(mut test: impl FnMut(&Database)) {
    test(&Database::open_sqlite_in_memory().unwrap());
    #[cfg(feature = "engine-store")]
    test(&Database::open_in_memory_on_engine().unwrap());
}
