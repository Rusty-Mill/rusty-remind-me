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

use std::io;
use std::path::{Path, PathBuf};

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
        let key = sha256::digest(bytes);
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
        let partial = path.with_extension(format!("partial-{}", std::process::id()));
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, &path)?;
        Ok(path)
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

    #[test]
    fn a_key_that_could_leave_the_root_is_refused() {
        let blobs = BlobStore::new("/nowhere");
        for key in ["", "../etc", "a/b", ".."] {
            let err = blobs.put_keyed(key, b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{key:?}");
        }
    }
}
