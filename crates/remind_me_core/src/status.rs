//! Operational status: is this working, and where is the data.
//!
//! # Reporting absence honestly
//!
//! `sync` and `dashboard` are subsystems this crate genuinely cannot answer
//! for on its own: the sync worker only exists once the MCP server process
//! instantiates one, and the dashboard is a wholly separate `rusty-remind-me
//! api` process this crate has no handle to. Both are reported here as
//! [`SubsystemStatus::NotImplemented`] carrying a reason, and both get
//! overridden with live state at the MCP dispatch layer (`remind_me_mcp`'s
//! `remind_me_server_status` arm), the same pattern `webhook`/`sync_peer`/
//! `remote` already use. A caller can tell "this crate can't see a
//! dashboard from here" apart from "the dashboard is down", which a bare
//! boolean cannot express.
//!
//! `embeddings` is different: the embedding backend's configuration
//! ([`crate::embedder::resolve_embedder`]) is read from environment
//! variables the same way in every process, so this module can and does
//! answer it directly — see below.
//!
//! # No network
//!
//! Nothing in `server_status` itself makes a network call. `embeddings`
//! reflects configuration only ([`crate::embedder::resolve_embedder`]); it
//! does not prove the configured backend is actually reachable, which needs
//! a network probe ([`crate::embedder::available_embedder`]) the reference
//! itself flags as a performance concern. The MCP dispatch layer calls
//! [`crate::embedder::embedding_status`] to add that live "and reachable"
//! check on top, same as it does for `dashboard`. A status tool that hangs
//! is worse than one that omits a field.

use crate::backup::{backup_dir, list_backups, BackupInfo};
use crate::db::migrations::SCHEMA_VERSION;
use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How a subsystem stands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubsystemStatus {
    Active,
    /// Present in `remind_me`, not built here yet. `reason` says so, so this
    /// is distinguishable from a subsystem that exists and is merely stopped.
    NotImplemented {
        reason: String,
    },
}

impl SubsystemStatus {
    fn missing(reason: &str) -> Self {
        Self::NotImplemented {
            reason: reason.to_string(),
        }
    }
}

/// A snapshot of where the data lives and what is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The build actually serving this request.
    ///
    /// Deliberately first: a stale install after a failed self-update explains
    /// more odd behaviour than anything else in this report, and it is the one
    /// fact a session otherwise has no way to observe — the reference makes the
    /// same argument for putting it on the first line of its own status output.
    pub version: String,
    /// `None` for an in-memory database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_path: Option<String>,
    pub database_exists: bool,
    pub database_bytes: Option<u64>,
    /// What `PRAGMA user_version` reports.
    pub schema_version: i32,
    /// What this build's generated schema corresponds to.
    pub expected_schema_version: i32,
    /// False when the two disagree — which is what makes a database
    /// unreadable to `remind_me`, so it is worth surfacing rather than
    /// inferring.
    pub schema_current: bool,
    pub memory_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<String>,
    pub backup_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_backup: Option<BackupInfo>,
    pub mcp: SubsystemStatus,
    pub dashboard: SubsystemStatus,
    pub embeddings: SubsystemStatus,
    pub sync: SubsystemStatus,
    /// Reported by the watcher itself now that one exists, rather than as an
    /// absent subsystem.
    pub watcher: crate::watcher::WatchStatus,
    /// Whether the reminder-delivery loop's thread is actually running, and
    /// at what interval. Unlike `watcher`, always answered directly rather
    /// than falling back to a config-only guess: the scheduler has no
    /// "configured or not" state to fall back through, only "running" or not
    /// (#270).
    pub scheduler: crate::scheduler::SchedulerStatus,
    /// Whether a stuck tool call would announce itself, and how many calls are
    /// in flight right now. Process-wide state, not database state — the
    /// reference reports it from the same tool for the same reason.
    pub watchdog: crate::watchdog::WatchdogStatus,
}

fn database_file(conn: &Connection) -> Result<Option<PathBuf>> {
    let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    Ok(if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    })
}

/// Collect the status snapshot.
///
/// Reports only what this crate actually has. Subsystems it lacks are named
/// with a reason rather than reported as stopped — shipping a field that reads
/// "not running" for something that was never built is the hollow-stub failure
/// mode, and it makes a real outage indistinguishable from an absent feature.
pub fn server_status(conn: &Connection) -> Result<ServerStatus> {
    let database_path = database_file(conn)?;
    let database_exists = database_path.as_ref().map(|p| p.exists()).unwrap_or(true); // in-memory: it exists, it just has no file
    let database_bytes = database_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());

    let schema_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let memory_count: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
        [],
        |row| row.get(0),
    )?;

    // An in-memory database has no directory to hold backups, so this is
    // absent rather than empty.
    let dir = backup_dir(conn).ok();
    let backups = dir
        .as_ref()
        .and_then(|d| list_backups(d).ok())
        .unwrap_or_default();

    Ok(ServerStatus {
        version: crate::updater::INSTALLED_VERSION.to_string(),
        database_path: database_path.map(|p| p.display().to_string()),
        database_exists,
        database_bytes,
        schema_version,
        expected_schema_version: SCHEMA_VERSION,
        schema_current: schema_version == SCHEMA_VERSION,
        memory_count,
        backup_dir: dir.map(|d| d.display().to_string()),
        backup_count: backups.len(),
        latest_backup: backups.into_iter().next(),
        mcp: SubsystemStatus::Active,
        dashboard: SubsystemStatus::missing(
            "dashboard liveness needs a cross-process PID-file check; see remind_me_mcp's \
             remind_me_server_status override",
        ),
        embeddings: if crate::embedder::resolve_embedder().is_some() {
            SubsystemStatus::Active
        } else {
            SubsystemStatus::missing(
                "no embedding backend configured; set REMIND_ME_EMBEDDING_BACKEND=ollama to \
                 enable semantic search",
            )
        },
        sync: SubsystemStatus::missing(
            "no sync engine in this crate; the outbox is written and pruned, never drained",
        ),
        // Same precedence as the tool surface: a running loop knows more than
        // a freshly-built one, which has never scanned anything (#203).
        watcher: crate::watcher::live_status()
            .or_else(|| crate::watcher::Watcher::from_env().map(|w| w.status()))
            .unwrap_or_else(crate::watcher::disabled_status),
        scheduler: crate::scheduler::live_status(),
        watchdog: crate::watchdog::status(),
    })
}
