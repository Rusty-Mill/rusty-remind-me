//! A content-addressed blob store: bytes in files under one root, named by
//! their sha256 (audit seam 3, D50).
//!
//! A blob for key `k` lives at `<root>/<k[0..2]>/<k>`; the two-character
//! shard keeps a directory listing usable after many thousands of blobs.
//! Writes are idempotent (an existing blob is left alone, since its name
//! is its content) and atomic (written beside the target, then renamed),
//! so a reader never sees a torn blob.
//!
//! Two users: raw transcript retention ([`crate::archive`], 16-hex keys,
//! unchanged on disk) and the R3 store benchmark, which keeps every page
//! and dialog body here with only metadata in the candidate's tables.
//! Production backups do not include a blob root yet (Q20).

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Blobs under one root directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// The store rooted at `root`. Nothing is created until a write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the blob for `key` lives (or would). A key shorter than two
    /// characters shards under `__` rather than panicking.
    pub fn path(&self, key: &str) -> PathBuf {
        let shard = key.get(..2).unwrap_or("__");
        self.root.join(shard).join(key)
    }

    /// Store `bytes` under their full sha256 (64 hex characters); the key.
    ///
    /// # Errors
    ///
    /// The filesystem's error.
    pub fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let key = content_key(bytes);
        self.put_keyed(&key, bytes)?;
        Ok(key)
    }

    /// Store `bytes` under a key the caller derived from them (archive
    /// uses a 16-hex sha256 prefix). Leaves an existing blob alone. Its
    /// path.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a key that is empty or not ASCII alphanumeric
    /// (it names a file), otherwise the filesystem's error.
    pub fn put_keyed(&self, key: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        check_key(key)?;
        let path = self.path(key);
        if path.exists() {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = self.stage(&path, bytes)?;
        match std::fs::rename(&partial, &path) {
            Ok(()) => Ok(path),
            // Another writer published the same key first (a rename onto
            // an existing file fails on Windows): its bytes are these.
            Err(_) if path.exists() => {
                std::fs::remove_file(&partial)?;
                Ok(path)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                Err(e)
            }
        }
    }

    /// Write `bytes` to a staging file beside `path` that this call alone
    /// created (`create_new`, a name no other writer in any process is
    /// given), so concurrent writers of one key never share it.
    fn stage(&self, path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let partial = path.with_extension(format!("partial-{}-{n}", std::process::id()));
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)
            {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            if let Err(e) = file.write_all(bytes) {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            return Ok(partial);
        }
    }

    /// Store `bytes` under their sha256 and return once the blob is durable
    /// and verified: the staged file is synced before it is renamed into
    /// place, the directory is synced after, and a blob already at the key
    /// is checked against its hash first. An existing blob that does not
    /// match (torn or altered) is replaced, since these bytes are what the
    /// key names. For a caller that commits a row pointing at the blob next
    /// (the dialog log); [`Self::put`] stays as it was for archive.
    ///
    /// Durability boundary: data and the directory entry are flushed with
    /// `sync_all` on the file and, on Unix, its directory. Windows has no
    /// directory sync, so there a power loss may still lose the rename; the
    /// row's own read-back check ([`crate::db::dialogs::DialogLog::body`])
    /// then reports the blob missing rather than returning wrong bytes.
    ///
    /// # Errors
    ///
    /// The filesystem's error; nothing is claimed durable on `Err`.
    pub fn put_durable(&self, bytes: &[u8]) -> io::Result<String> {
        let key = content_key(bytes);
        let path = self.path(&key);
        let parent = path.parent().map(Path::to_path_buf);
        if let Some(dir) = &parent {
            std::fs::create_dir_all(dir)?;
        }
        if self.verify(&key)? {
            OpenOptions::new().read(true).open(&path)?.sync_all()?;
        } else {
            let partial = self.stage(&path, bytes)?;
            let published = OpenOptions::new()
                .write(true)
                .open(&partial)
                .and_then(|f| f.sync_all())
                .and_then(|()| {
                    // A rename onto an existing file fails on Windows, and
                    // here the existing file is the corrupt one.
                    if path.exists() {
                        std::fs::remove_file(&path)?;
                    }
                    std::fs::rename(&partial, &path)
                });
            if let Err(e) = published {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
        }
        #[cfg(unix)]
        if let Some(dir) = &parent {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(key)
    }

    /// The bytes stored under `key`, or `None` when there is no such blob.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a malformed key, otherwise the filesystem's error.
    pub fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        check_key(key)?;
        match std::fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether the blob under `key` exists and its sha256 still starts with
    /// `key`: a missing or altered blob is `false`.
    ///
    /// # Errors
    ///
    /// As [`Self::get`].
    pub fn verify(&self, key: &str) -> io::Result<bool> {
        Ok(self
            .get(key)?
            .is_some_and(|bytes| sha256::digest(&bytes[..]).starts_with(key)))
    }
}

/// The key [`BlobStore::put`] gives `bytes`: their sha256, 64 hex
/// characters. Also what a caller hashes content with to compare it.
pub fn content_key(bytes: &[u8]) -> String {
    sha256::digest(bytes)
}

/// A key names a file, so it must be non-empty ASCII alphanumerics: no
/// separators, dots or `..`.
fn check_key(key: &str) -> io::Result<()> {
    if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("blob key {key:?} is not ASCII alphanumeric"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> BlobStore {
        let root = std::env::temp_dir().join(format!("blobs_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        BlobStore::new(root)
    }

    #[test]
    fn paths_shard_on_the_first_two_key_characters() {
        let blobs = BlobStore::new("/archive");
        assert_eq!(blobs.path("ab12cd34"), Path::new("/archive/ab/ab12cd34"));
        assert_eq!(blobs.path("a"), Path::new("/archive/__/a"));
    }

    #[test]
    fn put_keys_by_full_sha256_and_reads_back() {
        let blobs = scratch("put");
        let key = blobs.put(b"hello").unwrap();
        assert_eq!(key, sha256::digest("hello"));
        assert_eq!(blobs.get(&key).unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(blobs.verify(&key).unwrap());
        assert_eq!(blobs.put(b"hello").unwrap(), key, "idempotent");
        std::fs::remove_dir_all(blobs.root()).unwrap();
    }

    #[test]
    fn verify_catches_a_missing_or_altered_blob() {
        let blobs = scratch("verify");
        let key = blobs.put(b"body").unwrap();
        std::fs::write(blobs.path(&key), b"tampered").unwrap();
        assert!(!blobs.verify(&key).unwrap());
        assert!(!blobs.verify("00ff").unwrap());
        assert_eq!(blobs.get("00ff").unwrap(), None);
        std::fs::remove_dir_all(blobs.root()).unwrap();
    }

    #[test]
    fn a_keyed_put_takes_a_prefix_and_leaves_an_existing_blob() {
        let blobs = scratch("keyed");
        let key = &sha256::digest("raw")[..16];
        let path = blobs.put_keyed(key, b"raw").unwrap();
        assert!(blobs.verify(key).unwrap());
        blobs.put_keyed(key, b"other").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"raw");
        std::fs::remove_dir_all(blobs.root()).unwrap();
    }

    /// Many writers of one key at once: every put succeeds, the blob holds
    /// the bytes, and no staging file is left behind.
    #[test]
    fn concurrent_puts_of_one_key_all_succeed() {
        let blobs = scratch("concurrent");
        let body = vec![7u8; 64 * 1024];
        let key = content_key(&body);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| blobs.put_keyed(&key, &body).unwrap());
            }
        });
        assert_eq!(blobs.get(&key).unwrap().as_deref(), Some(&body[..]));
        assert!(blobs.verify(&key).unwrap());
        let shard = blobs.path(&key).parent().unwrap().to_path_buf();
        let names: Vec<_> = std::fs::read_dir(shard)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(&key)], "no staging left");
        std::fs::remove_dir_all(blobs.root()).unwrap();
    }

    #[test]
    fn a_key_that_could_leave_the_root_is_refused() {
        let blobs = BlobStore::new("/nowhere");
        for key in ["", "../etc", "a/b", ".."] {
            let err = blobs.put_keyed(key, b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{key:?}");
        }
    }
}
