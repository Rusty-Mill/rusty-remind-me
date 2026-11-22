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
//! A manifest hashes every row it covers, keyed by the row's identity:
//! each memory (all columns but the access and vitality counters, which
//! reads move), wiki page, chunk vector (`<memory>#<chunk>`), entity,
//! memory-entity link, entity relation and reference, and every body a
//! row links to, re-hashed from its blob. A changed row differs even when
//! every count is the same. Tables outside these groups (feedback,
//! promotions, sessions, outbox and the rest) are not covered; the R3
//! write-up names that scope.

use remind_me_core::blobs::{content_key, BlobStore};
use remind_me_core::db::entities::Entities;
use remind_me_core::db::memories::Memories;
use remind_me_core::db::references::References;
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::wiki::WikiIndex;
use remind_me_core::db::Store;
use remind_me_core::testing;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::bodies::{memory_link, page_link};
use super::Result;
use super::DIALOG;

/// Memory columns a read changes, left out of a row's hash.
const VOLATILE: [&str; 3] = ["access_count", "accessed_at", "vitality"];
/// Memories hashed per read.
const BATCH: usize = 500;

/// What a store holds, by content hash.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Memory id to the hash of its row.
    pub memories: BTreeMap<String, String>,
    /// Page slug to the hash of its row.
    pub pages: BTreeMap<String, String>,
    /// `<memory id>#<chunk index>` to the hash of that chunk's vector.
    pub vectors: BTreeMap<String, String>,
    /// Entity id to the hash of its row.
    pub entities: BTreeMap<String, String>,
    /// `<memory id>|<entity id>` to the hash of the link row.
    pub links: BTreeMap<String, String>,
    /// Relation id to the hash of its row.
    pub relations: BTreeMap<String, String>,
    /// Reference id to the hash of its row.
    pub references: BTreeMap<String, String>,
    /// Dialogs and pages with no link to an external body, which the body
    /// layout requires of every one (D50): `memory:<id>` or `page:<slug>`.
    pub unlinked: Vec<String>,
    /// Blob key a row links to, to the hash and size of its bytes, or
    /// `missing`.
    pub bodies: BTreeMap<String, String>,
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
                match memory_link(&memory.metadata) {
                    Some(key) => links.push(key),
                    None if memory.category == DIALOG => {
                        manifest.unlinked.push(format!("memory:{}", memory.id))
                    }
                    None => {}
                }
                manifest
                    .memories
                    .insert(memory.id, content_key(row.to_string().as_bytes()));
            }
        }
        for page in WikiIndex::new(store).recent_first()? {
            match page_link(&page.summary) {
                Some(key) => links.push(key),
                None => manifest.unlinked.push(format!("page:{}", page.slug)),
            }
            let row = serde_json::to_string(&page)?;
            manifest
                .pages
                .insert(page.slug, content_key(row.as_bytes()));
        }
        for id in &ids {
            for (ix, chunk) in Vectors::new(store).chunks_for(id)?.iter().enumerate() {
                manifest
                    .vectors
                    .insert(format!("{id}#{ix}"), content_key(&chunk.embedding));
            }
        }
        for chunk in ids.chunks(BATCH) {
            for reference in References::new(store).for_memories(chunk)? {
                let row = serde_json::to_string(&reference)?;
                manifest
                    .references
                    .insert(reference.id, content_key(row.as_bytes()));
            }
        }
        let entities = Entities::new(store);
        for entity in entities.all_oldest_first()? {
            let row = serde_json::to_string(&entity)?;
            manifest
                .entities
                .insert(entity.id, content_key(row.as_bytes()));
        }
        for (memory, entity, rest) in entities.links_oldest_first()? {
            let row = format!("{memory}\u{1f}{entity}\u{1f}{rest}");
            manifest
                .links
                .insert(format!("{memory}|{entity}"), content_key(row.as_bytes()));
        }
        for r in entities.relations_oldest_first()? {
            let row = [
                &r.id,
                &r.subject_entity_id,
                &r.relation,
                &r.object_entity_id,
                &r.created_at,
                &r.updated_at,
            ]
            .map(String::as_str)
            .join("\u{1f}");
            manifest.relations.insert(r.id, content_key(row.as_bytes()));
        }
        for key in links {
            let hash = match blobs.get(&key)? {
                Some(bytes) => format!("{} {}", content_key(&bytes), bytes.len()),
                None => "missing".into(),
            };
            manifest.bodies.insert(key, hash);
        }
        Ok(manifest)
    }

    /// Every section, by name.
    fn sections(&self) -> [(&'static str, &BTreeMap<String, String>); 8] {
        [
            ("memories", &self.memories),
            ("pages", &self.pages),
            ("vectors", &self.vectors),
            ("entities", &self.entities),
            ("links", &self.links),
            ("relations", &self.relations),
            ("references", &self.references),
            ("bodies", &self.bodies),
        ]
    }

    /// Items hashed, every section together.
    pub fn size(&self) -> usize {
        self.sections().iter().map(|(_, m)| m.len()).sum()
    }

    /// What differs from `other`, one line per section; empty when equal.
    /// A body whose blob is missing or altered differs even when both sides
    /// agree, since G8 needs every body present and byte-identical.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut found = Vec::new();
        for ((name, a), (_, b)) in self.sections().into_iter().zip(other.sections()) {
            let differing = a
                .keys()
                .chain(b.keys())
                .filter(|k| a.get(*k) != b.get(*k))
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            if differing > 0 {
                found.push(format!("{name}: {differing} differ"));
            }
        }
        let unverified = other
            .bodies
            .iter()
            .filter(|(key, hash)| !hash.starts_with(key.as_str()))
            .count();
        let unlinked = self.unlinked.len().max(other.unlinked.len());
        if unlinked > 0 {
            found.push(format!("{unlinked} dialogs or pages have no body link"));
        }
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
    fn a_dialog_without_its_body_link_fails_on_both_sides() {
        let mut m = manifest("abc 3");
        m.unlinked.push("memory:d1".into());
        let found = m.differences(&m.clone());
        assert!(
            found.iter().any(|d| d.contains("no body link")),
            "{found:?}"
        );
    }

    #[test]
    fn a_changed_relation_differs_at_the_same_count() {
        let mut source = manifest("abc 3");
        source
            .relations
            .insert("r1".into(), "subject relates object".into());
        let mut restored = manifest("abc 3");
        restored
            .relations
            .insert("r1".into(), "subject other object".into());
        assert_eq!(source.relations.len(), restored.relations.len());
        assert_eq!(source.differences(&restored), ["relations: 1 differ"]);
    }

    /// Read from a real store, every graph row and reference is keyed by
    /// identity, and swapping one relation for another (same count)
    /// changes the manifest.
    #[test]
    fn a_read_covers_the_graph_and_catches_a_changed_relation() {
        use remind_me_core::db::engine::EngineTables;
        use remind_me_core::db::references::NewReference;
        use remind_me_core::entity;
        use remind_me_core::models::{EntityInput, MemoryAddInput};
        use remind_me_core::Database;

        let db = Database::open_backend(Box::new(EngineTables::open_temporary().unwrap()), None)
            .unwrap();
        let store = db.store();
        let input = MemoryAddInput {
            content: "graph".into(),
            category: "fact".into(),
            ..Default::default()
        };
        let memory = remind_me_core::db::queries::add_memory(&store, input)
            .unwrap()
            .id;
        let upsert = |name: &str| {
            let input = EntityInput {
                name: name.into(),
                kind: None,
                aliases: vec![],
            };
            entity::upsert_entity(&store, &input).unwrap().id
        };
        let (a, b, c) = (upsert("a"), upsert("b"), upsert("c"));
        entity::link_memory_entity(&store, &memory, &a).unwrap();
        entity::upsert_entity_relation(&store, &a, "relates to", &b).unwrap();
        let reference = NewReference::new(&memory, "url", "https://example.com", "2026-11-22");
        References::new(&store).insert(&reference).unwrap();
        let blobs = BlobStore::new(std::env::temp_dir().join("package_test_no_blobs"));
        let before = Manifest::read(&store, &blobs).unwrap();
        let sizes = |m: &Manifest| {
            [
                m.entities.len(),
                m.links.len(),
                m.relations.len(),
                m.references.len(),
            ]
        };
        assert_eq!(sizes(&before), [3, 1, 1, 1]);

        // The relation now points at `c` instead of `b`: one row, changed.
        Entities::new(&store).repoint(&b, &c).unwrap();
        let after = Manifest::read(&store, &blobs).unwrap();
        assert_eq!(after.relations.len(), 1, "same count");
        let found = before.differences(&after);
        assert!(
            found.iter().any(|d| d.starts_with("relations: 1")),
            "{found:?}"
        );
    }

    #[test]
    fn a_body_altered_on_both_sides_still_fails() {
        let altered = manifest("ffff 3");
        assert!(!altered.differences(&manifest("ffff 3")).is_empty());
    }
}
