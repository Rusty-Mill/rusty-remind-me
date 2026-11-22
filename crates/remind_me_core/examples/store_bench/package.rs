//! G8's backup package and its isolated restore (D52).
//!
//! A package is one directory: `store/`, the candidate's own backup (the
//! engine's or SQLite's backup, or Attest's log files only); `blobs/`, a
//! copy of the run's blob root; and `manifest.json`. The parent keeps its
//! own copy of the manifest outside the package, moves the source run
//! away, and restores in a child process that sees only the package: it
//! copies the package into a fresh directory, opens it as a store, and
//! prints the manifest it reads back. The two manifests must be equal.
//!
//! A manifest hashes every memory (all columns but the access and
//! vitality counters, which reads move), every wiki page, every memory's
//! vectors, and every body a row links to, re-hashed from its blob, with
//! the counts of the tables beside them.

use remind_me_core::blobs::{content_key, BlobStore};
use remind_me_core::db::memories::Memories;
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::wiki::WikiIndex;
use remind_me_core::db::Store;
use remind_me_core::testing::{self, Table};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::bodies::{memory_link, page_link};
use super::Result;

/// Memory columns a read changes, left out of a row's hash.
const VOLATILE: [&str; 3] = ["access_count", "accessed_at", "vitality"];
/// Memories hashed per read.
const BATCH: usize = 500;

/// What a store holds, by content hash.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Memory id to the hash of its row.
    pub memories: BTreeMap<String, String>,
    /// Page slug to the hash of its row.
    pub pages: BTreeMap<String, String>,
    /// Memory id to the hash of its chunk vectors, in order.
    pub vectors: BTreeMap<String, String>,
    /// Blob key a row links to, to the hash and size of its bytes, or
    /// `missing`.
    pub bodies: BTreeMap<String, String>,
    /// Row counts of the tables the maps do not cover.
    pub counts: BTreeMap<String, i64>,
}

impl Manifest {
    /// Read `store` and the bodies its rows link to in `blobs`.
    pub fn read(store: &Store<'_>, blobs: &BlobStore) -> Result<Self> {
        let mut manifest = Self::default();
        let mut links = Vec::new();
        let ids = testing::memory_ids(store)?;
        for chunk in ids.chunks(BATCH) {
            for memory in Memories::new(store).get_many(chunk)? {
                let mut row = serde_json::to_value(&memory)?;
                if let Some(columns) = row.as_object_mut() {
                    for column in VOLATILE {
                        columns.remove(column);
                    }
                }
                links.extend(memory_link(&memory.metadata));
                manifest
                    .memories
                    .insert(memory.id, content_key(row.to_string().as_bytes()));
            }
        }
        for page in WikiIndex::new(store).recent_first()? {
            links.extend(page_link(&page.summary));
            let row = serde_json::to_string(&page)?;
            manifest
                .pages
                .insert(page.slug, content_key(row.as_bytes()));
        }
        let mut vectors: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for chunk in Vectors::new(store).all()? {
            vectors
                .entry(chunk.memory_id)
                .or_default()
                .extend(chunk.embedding);
        }
        manifest.vectors = vectors
            .into_iter()
            .map(|(id, bytes)| (id, content_key(&bytes)))
            .collect();
        for key in links {
            let hash = match blobs.get(&key)? {
                Some(bytes) => format!("{} {}", content_key(&bytes), bytes.len()),
                None => "missing".into(),
            };
            manifest.bodies.insert(key, hash);
        }
        for (name, table) in [
            ("entities", Table::Entities),
            ("memory_entities", Table::MemoryEntities),
            ("entity_relations", Table::EntityRelations),
            ("vec_chunks", Table::VecChunks),
        ] {
            manifest
                .counts
                .insert(name.into(), testing::count(store, table)?);
        }
        Ok(manifest)
    }

    /// Rows, pages, vectors and bodies.
    pub fn size(&self) -> usize {
        self.memories.len() + self.pages.len() + self.vectors.len() + self.bodies.len()
    }

    /// What differs from `other`, one line per section; empty when equal.
    /// A body whose blob is missing or altered differs even when both sides
    /// agree, since G8 needs every body present and byte-identical.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut found = Vec::new();
        let mut section =
            |name: &str, a: &BTreeMap<String, String>, b: &BTreeMap<String, String>| {
                let differing = a
                    .keys()
                    .chain(b.keys())
                    .filter(|k| a.get(*k) != b.get(*k))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                if differing > 0 {
                    found.push(format!("{name}: {differing} differ"));
                }
            };
        section("memories", &self.memories, &other.memories);
        section("pages", &self.pages, &other.pages);
        section("vectors", &self.vectors, &other.vectors);
        section("bodies", &self.bodies, &other.bodies);
        if self.counts != other.counts {
            found.push(format!("counts: {:?} vs {:?}", self.counts, other.counts));
        }
        let unverified = other
            .bodies
            .iter()
            .filter(|(key, hash)| !hash.starts_with(key.as_str()))
            .count();
        if unverified > 0 {
            found.push(format!("bodies: {unverified} missing or altered"));
        }
        found
    }
}

/// Copy the directory `from` to `to`, which must not exist, leaving out
/// files named in `skip` (a lock file that names its holder).
pub fn copy_dir(from: &Path, to: &Path, skip: &[&str]) -> Result<()> {
    std::fs::create_dir(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target, skip)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body_hash: &str) -> Manifest {
        let mut m = Manifest::default();
        m.memories.insert("a".into(), "h1".into());
        m.bodies.insert("abc".into(), body_hash.into());
        m
    }

    #[test]
    fn equal_manifests_with_verified_bodies_have_no_differences() {
        let m = manifest("abc 3");
        assert!(m.differences(&manifest("abc 3")).is_empty());
    }

    #[test]
    fn a_changed_row_or_a_missing_body_is_reported() {
        let source = manifest("abc 3");
        let mut restored = manifest("missing");
        restored.memories.insert("a".into(), "h2".into());
        let found = source.differences(&restored);
        assert!(
            found.iter().any(|d| d.starts_with("memories: 1")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|d| d.contains("missing or altered")),
            "{found:?}"
        );
    }

    #[test]
    fn a_body_altered_on_both_sides_still_fails() {
        let altered = manifest("ffff 3");
        assert!(!altered.differences(&manifest("ffff 3")).is_empty());
    }
}
