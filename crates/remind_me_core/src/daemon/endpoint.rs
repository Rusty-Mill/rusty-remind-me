//! The files beside the database that let clients find the daemon.
//!
//! Each is named after the database file (`remind_me.db.daemon.json` for
//! `remind_me.db`), so two stores in one directory get two daemons rather
//! than one serving both.
//!
//! - `.daemon.lock`: held by the running daemon for its whole life. The OS
//!   releases it when the process dies, however it dies, so a crashed daemon
//!   never blocks the next one.
//! - `.daemon.token`: the shared secret a client presents first. Mode 600 on
//!   unix, like `remote`'s connector token, and new on every start.
//! - `.daemon.json`: where the daemon listens. Written last, after the token,
//!   so a client that can read it can also authenticate.
//! - `.daemon.log`: the daemon's stderr when a client started it.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Where one database's daemon files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    dir: PathBuf,
    /// The database's file name, which every daemon file starts with.
    stem: String,
}

/// What `daemon.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub build: String,
    pub started_at: String,
}

impl Endpoint {
    /// The endpoint for the database at `db_path`.
    pub fn for_db(db_path: &Path) -> Self {
        let dir = db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let stem = db_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "remind_me".to_string());
        Self {
            dir: dir.to_path_buf(),
            stem,
        }
    }

    fn file(&self, suffix: &str) -> PathBuf {
        self.dir.join(format!("{}.daemon.{suffix}", self.stem))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.file("lock")
    }

    pub fn info_path(&self) -> PathBuf {
        self.file("json")
    }

    pub fn token_path(&self) -> PathBuf {
        self.file("token")
    }

    pub fn log_path(&self) -> PathBuf {
        self.file("log")
    }

    /// Whether the store is being copied onto the engine: the copy's partial
    /// directory exists and the engine directory does not yet. The daemon
    /// runs that one-time copy before it listens.
    pub fn copying_onto_engine(&self) -> bool {
        let engine = crate::db::engine_dir(&self.dir.join(&self.stem));
        !engine.exists() && crate::db::partial_dir(&engine).exists()
    }

    /// Take the daemon lock, or `None` when another daemon holds it.
    ///
    /// The returned file must be kept open for as long as the daemon runs.
    pub fn try_lock(&self) -> io::Result<Option<File>> {
        fs::create_dir_all(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// Write a fresh token, then `info`. Returns the token.
    pub fn publish(&self, info: &DaemonInfo) -> io::Result<String> {
        let token = crate::remote::generate_token();
        write_private(&self.token_path(), token.as_bytes())?;
        let json = serde_json::to_vec(info).map_err(io::Error::other)?;
        write_private(&self.info_path(), &json)?;
        Ok(token)
    }

    /// Remove `daemon.json`, if it still names this process.
    pub fn withdraw(&self, pid: u32) {
        if self.read_info().is_some_and(|info| info.pid == pid) {
            let _ = fs::remove_file(self.info_path());
        }
    }

    pub fn read_info(&self) -> Option<DaemonInfo> {
        let raw = fs::read(self.info_path()).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn read_token(&self) -> Option<String> {
        let token = fs::read_to_string(self.token_path()).ok()?;
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_string())
    }
}

/// Replace `path` with `contents`, readable only by this user on unix.
///
/// Written to a sibling and renamed, so a reader sees the old file or the
/// new one, never half of either.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    rusty_atomic_file::write_private(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_endpoint(tag: &str) -> Endpoint {
        let dir = std::env::temp_dir().join(format!(
            "rrm_endpoint_{tag}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        Endpoint::for_db(&dir.join("memory.db"))
    }

    fn info(pid: u32) -> DaemonInfo {
        DaemonInfo {
            pid,
            port: 4242,
            build: "b".into(),
            started_at: "now".into(),
        }
    }

    #[test]
    fn the_lock_is_exclusive_until_released() {
        let endpoint = temp_endpoint("lock");
        let held = endpoint.try_lock().unwrap().expect("first lock");
        assert!(endpoint.try_lock().unwrap().is_none());
        drop(held);
        // A child another test thread forks holds a copy of the lock file's
        // descriptor until it execs, and an `flock` lock lasts while any copy
        // is open: the release can take a moment to show.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut again = endpoint.try_lock().unwrap();
        while again.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            again = endpoint.try_lock().unwrap();
        }
        assert!(
            again.is_some(),
            "the lock is released once its holder drops it"
        );
    }

    #[test]
    fn publish_then_read_back() {
        let endpoint = temp_endpoint("publish");
        fs::create_dir_all(&endpoint.dir).unwrap();
        let token = endpoint.publish(&info(7)).unwrap();
        assert_eq!(endpoint.read_token().as_deref(), Some(token.as_str()));
        assert_eq!(endpoint.read_info(), Some(info(7)));
        let again = endpoint.publish(&info(7)).unwrap();
        assert_ne!(token, again, "every start gets a new token");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let endpoint = temp_endpoint("mode");
        fs::create_dir_all(&endpoint.dir).unwrap();
        endpoint.publish(&info(1)).unwrap();
        let mode = fs::metadata(endpoint.token_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn two_stores_in_one_directory_get_their_own_files() {
        let dir = std::env::temp_dir().join("rrm_endpoint_pair");
        let a = Endpoint::for_db(&dir.join("a.db"));
        let b = Endpoint::for_db(&dir.join("b.db"));
        assert_ne!(a.lock_path(), b.lock_path());
        assert_eq!(a.info_path(), dir.join("a.db.daemon.json"));
    }

    #[test]
    fn withdraw_leaves_another_daemons_info() {
        let endpoint = temp_endpoint("withdraw");
        fs::create_dir_all(&endpoint.dir).unwrap();
        endpoint.publish(&info(1)).unwrap();
        endpoint.withdraw(2);
        assert!(endpoint.read_info().is_some());
        endpoint.withdraw(1);
        assert!(endpoint.read_info().is_none());
    }

    #[test]
    fn a_copy_is_under_way_while_only_its_partial_directory_exists() {
        let endpoint = temp_endpoint("copying");
        let engine = endpoint.dir.join("memory.engine");
        let partial = endpoint.dir.join("memory.engine.partial");
        fs::create_dir_all(&endpoint.dir).unwrap();
        assert!(!endpoint.copying_onto_engine(), "nothing copied yet");
        fs::create_dir_all(&partial).unwrap();
        assert!(endpoint.copying_onto_engine());
        // The copy renames the partial directory into place when it is done.
        fs::rename(&partial, &engine).unwrap();
        assert!(!endpoint.copying_onto_engine(), "the copy finished");
        fs::create_dir_all(&partial).unwrap();
        assert!(
            !endpoint.copying_onto_engine(),
            "a store already on the engine is never copied again"
        );
        fs::remove_dir_all(&endpoint.dir).unwrap();
    }
}
