//! The verbatim dialog store (#528, D76): append and read only.
//!
//! A dialog is the source of truth; memories are projections of it. This
//! port has no update, delete, merge or prune, so "never cleaned" is a fact
//! of the type, not a convention. A row keeps a short head (D51) and the
//! sha256 of the full body, which lives in a [`BlobStore`] (D50).
//!
//! [`DialogLog`] is the one write path: body to the blob store first, then
//! the row, and only after the blob is durable and verified
//! ([`BlobStore::put_durable`]: synced file, the shard, root and anchor directories flushed on every call, an existing blob
//! checked against its hash and replaced if it fails). A crash between the
//! two leaves an orphan blob and no dangling row; a failed publication
//! appends nothing. A rerun is idempotent by body hash.

use crate::blobs::{content_key, BlobStore};
use crate::db::{Result, StoreError};

/// Bytes of the body kept in the row, cut back to a UTF-8 boundary (D51).
pub const HEAD_BYTES: usize = 512;

/// One stored dialog: metadata and head; the body is in the blob store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogRecord {
    pub id: String,
    pub capture_id: String,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub created_at: String,
    pub head: String,
    /// Full sha256 hex of the body: its key in the blob store.
    pub body_hash: String,
    pub body_bytes: u64,
}

/// What [`DialogStore::append`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appended {
    Inserted,
    /// The identical record was already there; nothing changed.
    Duplicate,
}

/// Append-only access to stored dialogs.
pub trait DialogStore {
    /// Add `record`. The same id with identical content is
    /// [`Appended::Duplicate`]; the same id with different content is
    /// [`StoreError::Invalid`] and changes nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError`] on a refused or failed write.
    fn append(&mut self, record: &DialogRecord) -> Result<Appended>;
    /// The dialog with `id`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the store cannot be read.
    fn get(&self, id: &str) -> Result<Option<DialogRecord>>;
    /// Dialogs of one capture, oldest first (ties by id).
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the store cannot be read.
    fn for_capture(&self, capture_id: &str) -> Result<Vec<DialogRecord>>;
    /// How many dialogs are stored.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the store cannot be read.
    fn count(&self) -> Result<u64>;
}

/// The first [`HEAD_BYTES`] of `body`, ending on a character boundary.
pub fn head_of(body: &str) -> &str {
    let mut end = body.len().min(HEAD_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// Writes whole dialogs: body to blobs, then the row.
pub struct DialogLog<S: DialogStore> {
    store: S,
    blobs: BlobStore,
}

/// Fields of a dialog the caller supplies; the log derives the rest.
pub struct NewDialog<'a> {
    pub id: &'a str,
    pub capture_id: &'a str,
    pub session_id: Option<&'a str>,
    pub project: Option<&'a str>,
    pub created_at: &'a str,
    pub body: &'a str,
}

impl<S: DialogStore> DialogLog<S> {
    pub fn new(store: S, blobs: BlobStore) -> Self {
        Self { store, blobs }
    }

    /// Store `dialog` verbatim (the caller has already redacted, D15).
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the blob cannot be written, otherwise the
    /// store's error.
    pub fn record(&mut self, dialog: &NewDialog<'_>) -> Result<Appended> {
        let hash = self
            .blobs
            .put_durable(dialog.body.as_bytes())
            .map_err(|e| StoreError::Engine(format!("dialog blob: {e}")))?;
        self.store.append(&DialogRecord {
            id: dialog.id.to_owned(),
            capture_id: dialog.capture_id.to_owned(),
            session_id: dialog.session_id.map(str::to_owned),
            project: dialog.project.map(str::to_owned),
            created_at: dialog.created_at.to_owned(),
            head: head_of(dialog.body).to_owned(),
            body_hash: hash,
            body_bytes: dialog.body.len() as u64,
        })
    }

    /// The full body of `record`, checked against its hash.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the blob is missing, unreadable or does
    /// not match its hash.
    pub fn body(&self, record: &DialogRecord) -> Result<String> {
        let bytes = self
            .blobs
            .get(&record.body_hash)
            .map_err(|e| StoreError::Engine(format!("dialog blob: {e}")))?
            .ok_or_else(|| StoreError::Engine(format!("dialog {} has no blob", record.id)))?;
        if content_key(&bytes) != record.body_hash {
            return Err(StoreError::Engine(format!(
                "dialog {} blob does not match its hash",
                record.id
            )));
        }
        String::from_utf8(bytes)
            .map_err(|_| StoreError::Engine(format!("dialog {} body is not UTF-8", record.id)))
    }

    pub fn store(&self) -> &S {
        &self.store
    }
}

/// Behaviour every [`DialogStore`] must show; a backend's tests run it
/// through [`dialog_conformance!`](crate::dialog_conformance).
#[doc(hidden)]
#[cfg(test)]
pub mod conformance {
    use super::*;

    pub fn record(id: &str, capture: &str, at: &str, body: &str) -> DialogRecord {
        DialogRecord {
            id: id.into(),
            capture_id: capture.into(),
            session_id: Some("s1".into()),
            project: None,
            created_at: at.into(),
            head: head_of(body).into(),
            body_hash: content_key(body.as_bytes()),
            body_bytes: body.len() as u64,
        }
    }

    pub fn round_trips_every_field(s: &mut dyn DialogStore) {
        let r = record("d1", "c1", "2026-11-25T00:00:00Z", "hello");
        assert_eq!(s.append(&r).unwrap(), Appended::Inserted);
        assert_eq!(s.get("d1").unwrap(), Some(r));
        assert_eq!(s.get("nope").unwrap(), None);
        assert_eq!(s.count().unwrap(), 1);
    }

    pub fn the_same_record_twice_is_a_duplicate(s: &mut dyn DialogStore) {
        let r = record("d1", "c1", "2026-11-25T00:00:00Z", "hello");
        s.append(&r).unwrap();
        assert_eq!(s.append(&r).unwrap(), Appended::Duplicate);
        assert_eq!(s.count().unwrap(), 1);
    }

    pub fn an_id_is_never_rewritten(s: &mut dyn DialogStore) {
        let r = record("d1", "c1", "2026-11-25T00:00:00Z", "hello");
        s.append(&r).unwrap();
        let other = record("d1", "c1", "2026-11-25T00:00:00Z", "changed");
        assert!(matches!(s.append(&other), Err(StoreError::Invalid(_))));
        assert_eq!(s.get("d1").unwrap(), Some(r));
    }

    pub fn for_capture_is_oldest_first(s: &mut dyn DialogStore) {
        s.append(&record("b", "c1", "2026-11-25T00:00:02Z", "2"))
            .unwrap();
        s.append(&record("a", "c1", "2026-11-25T00:00:01Z", "1"))
            .unwrap();
        s.append(&record("z", "c2", "2026-11-25T00:00:00Z", "x"))
            .unwrap();
        let ids: Vec<_> = s
            .for_capture("c1")
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(s.for_capture("none").unwrap().is_empty());
    }
}

/// Runs the [`DialogStore`] conformance checks against `$make`, a
/// constructor expression for a fresh store.
#[doc(hidden)]
#[macro_export]
macro_rules! dialog_conformance {
    ($make:expr) => {
        use $crate::db::dialogs::conformance;

        #[test]
        fn dialog_round_trips_every_field() {
            conformance::round_trips_every_field(&mut $make);
        }
        #[test]
        fn dialog_the_same_record_twice_is_a_duplicate() {
            conformance::the_same_record_twice_is_a_duplicate(&mut $make);
        }
        #[test]
        fn dialog_an_id_is_never_rewritten() {
            conformance::an_id_is_never_rewritten(&mut $make);
        }
        #[test]
        fn dialog_for_capture_is_oldest_first() {
            conformance::for_capture_is_oldest_first(&mut $make);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_cuts_on_a_char_boundary() {
        let body = "é".repeat(400); // 800 bytes
        let head = head_of(&body);
        assert_eq!(head.len(), 512);
        let odd = format!("a{}", "é".repeat(400)); // boundary at 511, not 512
        assert_eq!(head_of(&odd).len(), 511);
        assert_eq!(head_of("short"), "short");
    }
}
