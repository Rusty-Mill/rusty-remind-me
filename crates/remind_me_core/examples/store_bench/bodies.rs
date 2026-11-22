//! R3's body layout (D40/D41/D43, D50, D51), the same for every candidate
//! because the harness, not the candidate, applies it: each dialog and page
//! body goes whole to a content-addressed [`BlobStore`] under the run, and
//! the candidate's tables get only a head of at most [`HEAD_BYTES`] and a
//! link to the blob.
//!
//! A dialog memory carries the link as `metadata.source_artifact`
//! (`{store: "blobs", id, hash}`, both the body's sha256) with
//! `metadata.body_bytes`; a wiki page, which has no metadata, carries it as
//! its summary, `blob:<sha256>`. Search therefore sees heads only, the same
//! on every candidate (D51).

use remind_me_core::blobs::BlobStore;
use remind_me_core::source_artifact::{SourceArtifact, METADATA_KEY};
use serde_json::{json, Value};
use std::path::PathBuf;

use super::Result;

/// Most bytes of a body a row keeps (D51).
pub const HEAD_BYTES: usize = 512;
/// The `source_artifact.store` naming the run's blob root.
pub const STORE: &str = "blobs";
/// What a page's summary starts with before its blob key.
pub const PAGE_LINK: &str = "blob:";

/// A body moved to the blob store: what the row keeps instead.
pub struct Stashed {
    /// The body's first bytes, at most [`HEAD_BYTES`], on a char boundary.
    pub head: String,
    /// The blob's key, the body's sha256.
    pub key: String,
    /// The body's length in bytes.
    pub bytes: usize,
}

impl Stashed {
    /// A dialog row's metadata: the link and the body's length.
    pub fn metadata(&self) -> Value {
        let link = SourceArtifact {
            store: STORE.into(),
            id: self.key.clone(),
            hash: self.key.clone(),
        };
        json!({ METADATA_KEY: link.to_value(), "body_bytes": self.bytes })
    }

    /// A page row's summary: the link.
    pub fn page_summary(&self) -> String {
        format!("{PAGE_LINK}{}", self.key)
    }
}

/// The run's blob root, where every body goes.
pub struct Bodies {
    blobs: BlobStore,
}

impl Bodies {
    /// Bodies under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            blobs: BlobStore::new(root),
        }
    }

    /// The blob store.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Store `body` whole; the head and link a row keeps.
    pub fn stash(&self, body: &str) -> Result<Stashed> {
        let key = self.blobs.put(body.as_bytes())?;
        Ok(Stashed {
            head: head(body).to_string(),
            key,
            bytes: body.len(),
        })
    }
}

/// The blob key a memory's metadata links to, if it links to this store.
pub fn memory_link(metadata: &Value) -> Option<String> {
    SourceArtifact::from_metadata(metadata)
        .filter(|link| link.store == STORE)
        .map(|link| link.hash)
}

/// The blob key a page's summary links to, if any.
pub fn page_link(summary: &str) -> Option<String> {
    summary.strip_prefix(PAGE_LINK).map(str::to_string)
}

/// The first [`HEAD_BYTES`] of `body`, cut back to a char boundary.
fn head(body: &str) -> &str {
    let mut end = body.len().min(HEAD_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_is_whole_when_short_and_cut_on_a_char_boundary() {
        assert_eq!(head("short"), "short");
        let long = "é".repeat(400); // 800 bytes, two per char
        assert_eq!(head(&long).len(), HEAD_BYTES);
        let odd = format!("a{}", "é".repeat(400)); // boundary at 511
        assert_eq!(head(&odd).len(), HEAD_BYTES - 1);
    }

    #[test]
    fn a_stashed_body_links_back_from_metadata_and_summary() {
        let root = std::env::temp_dir().join(format!("bodies_{}", std::process::id()));
        let bodies = Bodies::new(&root);
        let body = "x".repeat(2_000);
        let stashed = bodies.stash(&body).unwrap();
        assert_eq!(stashed.head.len(), HEAD_BYTES);
        assert_eq!(stashed.bytes, 2_000);
        let metadata = stashed.metadata();
        assert_eq!(memory_link(&metadata).as_deref(), Some(&stashed.key[..]));
        assert_eq!(metadata["body_bytes"], 2_000);
        assert_eq!(
            page_link(&stashed.page_summary()),
            Some(stashed.key.clone())
        );
        assert_eq!(
            bodies.blobs().get(&stashed.key).unwrap().as_deref(),
            Some(body.as_bytes())
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
