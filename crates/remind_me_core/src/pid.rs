//! PID-file liveness tracking for the dashboard (`rusty-remind-me api`)
//! process — the Rust equivalent of the reference's `remind_me_mcp/pid.py`.
//!
//! # Cross-process by necessity
//!
//! The dashboard runs as its own OS process, separate from whichever MCP
//! server process answers `remind_me_server_status`. There is no in-process
//! handle to ask, unlike the webhook or sync worker (both owned directly by
//! the MCP server). A JSON file beside the database — written by the
//! dashboard on start, read by anyone asking "is it up" — is the only shared
//! state the two processes have.
//!
//! # Two checks, not one
//!
//! A PID file merely existing does not mean the process it names is still
//! running, or still serving this dashboard rather than something else. Two
//! independent signals both have to hold before this reports "running":
//!
//! 1. The file parses as a well-formed [`PidRecord`].
//! 2. `GET {url}/health` answers `200` inside [`HEALTH_TIMEOUT`].
//!
//! The reference also pre-filters with `os.kill(pid, 0)` before attempting
//! the HTTP probe, as a cheap way to avoid a network round-trip against a
//! definitely-dead process. That check needs a libc binding this workspace
//! does not otherwise depend on, so it is skipped here — every path that
//! would make it matter (a dead process, a hung process, a port reused by
//! something else) still resolves correctly through the health check alone,
//! just with a probe that was always going to happen anyway once the PID
//! file's mere presence stopped being trusted on its own.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Filename written beside the database file — mirrors `pid.py`'s
/// `PID_FILE` living beside `DB_PATH` in the same `MEMORY_DIR`. Deriving the
/// location from the database's own path (see [`pid_file_path`]) gives
/// "double-start refusal for the same DB" for free: two dashboards pointed
/// at the same database resolve to the same PID file.
const PID_FILE_NAME: &str = "server.pid";

/// Matches the reference's `_check_ui_server_health` timeout exactly.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum PidError {
    #[error("this database is in memory and has no on-disk location for a PID file")]
    InMemory,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, PidError>;

/// Where the main database lives on disk. Deliberately re-derived here
/// rather than shared with `backup::backup_dir`/`status::database_file` —
/// each of those already keeps its own private copy of this one-line
/// `PRAGMA` query rather than a shared helper, and this follows the same
/// established shape.
fn database_path(conn: &Connection) -> rusqlite::Result<Option<PathBuf>> {
    let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    Ok(if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    })
}

/// The PID file's path, beside the database file.
pub fn pid_file_path(conn: &Connection) -> Result<PathBuf> {
    let db_path = database_path(conn)?.ok_or(PidError::InMemory)?;
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(PID_FILE_NAME))
}

/// What gets written to, and read back from, the PID file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub url: String,
    pub started_at: String,
}

/// Write the PID file for the current process — called right after the
/// dashboard server binds its listener.
pub fn write_pid_file(path: &Path, host: &str, port: u16) -> Result<PidRecord> {
    let record = PidRecord {
        pid: std::process::id(),
        host: host.to_string(),
        port,
        url: format!("http://{host}:{port}"),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    // `to_string_pretty` on a plain struct of strings/ints cannot fail.
    let json = serde_json::to_string_pretty(&record).unwrap_or_default();
    std::fs::write(path, json).map_err(|source| PidError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(record)
}

/// Remove the PID file. Safe to call even if it does not exist — mirrors
/// `_remove_pid_file`'s `missing_ok=True`, since this runs on every shutdown
/// path including ones where the file was never written.
pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Parse the PID file, if one exists and is well-formed.
///
/// A malformed file (not JSON, or missing a field) is removed on the spot
/// and reported as absent — matching `_read_pid_file`'s own cleanup of a
/// `json.JSONDecodeError`/`KeyError`/`TypeError`. A file that simply is not
/// there is reported as absent without touching the filesystem again.
fn read_pid_record(path: &Path) -> Option<PidRecord> {
    let contents = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(record) => Some(record),
        Err(_) => {
            remove_pid_file(path);
            None
        }
    }
}

/// `GET {url}/health` with a short timeout, `true` only on an HTTP 200.
///
/// Hand-rolled over `std::net`, the same shape as
/// [`crate::embedder::OllamaEmbedder`]'s HTTP client — this workspace's
/// established way to avoid taking on an HTTP client dependency for a single
/// small synchronous call.
fn check_health(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = rest.split_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Some(addr) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, HEALTH_TIMEOUT) else {
        return false;
    };
    stream.set_read_timeout(Some(HEALTH_TIMEOUT)).ok();
    stream.set_write_timeout(Some(HEALTH_TIMEOUT)).ok();

    let request = format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut raw = Vec::new();
    if stream.read_to_end(&mut raw).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&raw);
    let Some((head, _)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    head.lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        == Some(200)
}

/// What `remind_me_server_status` reports for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub pid_file: String,
}

/// Combine the PID file and a live health check to decide whether the
/// dashboard is actually running — matching the reference's
/// `get_server_status()` exactly: `info and _check_ui_server_health(...)`.
///
/// A PID file that fails the health check (the process died without
/// cleaning up, or something unrelated now owns that port) is treated as
/// stale and removed, same as the reference deleting it inside
/// `_read_pid_file` — except the staleness signal here is "didn't answer
/// health", not "kill(0) failed". See the module docs for why.
pub fn dashboard_status(path: &Path) -> DashboardStatus {
    let pid_file = path.display().to_string();
    match read_pid_record(path) {
        Some(record) if check_health(&record.url) => DashboardStatus {
            running: true,
            url: Some(record.url),
            pid: Some(record.pid),
            started_at: Some(record.started_at),
            pid_file,
        },
        Some(_stale) => {
            remove_pid_file(path);
            DashboardStatus {
                running: false,
                url: None,
                pid: None,
                started_at: None,
                pid_file,
            }
        }
        None => DashboardStatus {
            running: false,
            url: None,
            pid: None,
            started_at: None,
            pid_file,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pid_file_reports_not_running() {
        let dir = std::env::temp_dir().join(format!("rrm_pid_none_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.pid");

        let status = dashboard_status(&path);

        assert!(!status.running);
        assert!(status.url.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_pid_file_is_removed_and_reported_not_running() {
        let dir = std::env::temp_dir().join(format!("rrm_pid_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.pid");
        std::fs::write(&path, "not json").unwrap();

        let status = dashboard_status(&path);

        assert!(!status.running);
        assert!(!path.exists(), "malformed pid file should be cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }
}
