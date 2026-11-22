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
//! reads move), wiki page and reference; every entity, memory-entity
//! link, entity relation and chunk vector as stored, read through
//! `testing::stored_rows` so columns the ports leave out count too (an
//! entity's or relation's `node_id`, a chunk's stored `chunk_ix`, which
//! keys it as `<memory>#<chunk_ix>`); and every body a row links to,
//! re-hashed from its blob. A changed row differs even when
//! every count is the same. Tables outside these groups (feedback,
//! promotions, sessions, outbox and the rest) are not covered; the R3
//! write-up names that scope.

use remind_me_core::blobs::{content_key, BlobStore};
use remind_me_core::db::memories::Memories;
use remind_me_core::db::references::References;
use remind_me_core::db::wiki::WikiIndex;
use remind_me_core::db::Store;
use remind_me_core::testing::{self, Table};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
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
    /// `<memory id>#<stored chunk_ix>` to the hash of that chunk's stored
    /// row: memory id, chunk index and embedding.
    pub vectors: BTreeMap<String, String>,
    /// Entity id to the hash of its stored row, `node_id` included.
    pub entities: BTreeMap<String, String>,
    /// `<memory id>#<entity id>` to the hash of the stored link row.
    pub links: BTreeMap<String, String>,
    /// Relation id to the hash of its stored row, `node_id` included.
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
        for chunk in ids.chunks(BATCH) {
            for reference in References::new(store).for_memories(chunk)? {
                let row = serde_json::to_string(&reference)?;
                manifest
                    .references
                    .insert(reference.id, content_key(row.as_bytes()));
            }
        }
        // The graph and chunk rows as stored, every column included, so a
        // row's `node_id` or a chunk's stored `chunk_ix` is covered too.
        let tables = [
            (Table::Entities, &mut manifest.entities, &["id"][..]),
            (
                Table::MemoryEntities,
                &mut manifest.links,
                &["memory_id", "entity_id"][..],
            ),
            (Table::EntityRelations, &mut manifest.relations, &["id"][..]),
            (
                Table::VecChunks,
                &mut manifest.vectors,
                &["memory_id", "chunk_ix"][..],
            ),
        ];
        for (table, section, identity) in tables {
            for row in testing::stored_rows(store, table)? {
                section.insert(identity_of(&row, identity), row_hash(&row));
            }
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

/// A stored row's identity: its `columns`, joined by `#`
/// (`<memory>#<chunk index>`, `<memory>#<entity>`).
fn identity_of(row: &Map<String, Value>, columns: &[&str]) -> String {
    columns
        .iter()
        .map(|c| match row.get(*c) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .collect::<Vec<_>>()
        .join("#")
}

/// The hash of a stored row, its columns in name order.
pub fn row_hash(row: &Map<String, Value>) -> String {
    let sorted: BTreeMap<&String, &Value> = row.iter().collect();
    content_key(json!(sorted).to_string().as_bytes())
}

/// Copy the directory `from` to `to`, which must not exist and is made
/// private to this user, leaving out files named in `skip` (a lock file
/// that names its holder). The whole of `from` is checked first: anything
/// but plain files and directories (a symlink, a device) is refused before
/// anything is written, so a copy never follows a link out of its source.
pub fn copy_dir(from: &Path, to: &Path, skip: &[&str]) -> Result<()> {
    check_tree(from)?;
    private_dir(to)?;
    copy_tree(from, to, skip)
}

/// Make the directory `dir`, which must not exist, readable by this user
/// only (on Unix).
pub fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Refuse a tree holding anything but plain files and directories.
fn check_tree(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let kind = std::fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            check_tree(&path)?;
        } else if !kind.is_file() {
            return Err(format!("{} is not a plain file or directory", path.display()).into());
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path, skip: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target, skip)?;
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
        use remind_me_core::db::entities::Entities;
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

    /// An engine store for the stored-row cases below.
    fn engine() -> remind_me_core::Database {
        use remind_me_core::db::engine::EngineTables;
        let tables = EngineTables::open_temporary().unwrap();
        remind_me_core::Database::open_backend(Box::new(tables), None).unwrap()
    }

    fn no_blobs() -> BlobStore {
        BlobStore::new(std::env::temp_dir().join("package_test_no_blobs"))
    }

    /// The same entity content written by another node is a changed row.
    #[test]
    fn a_changed_node_id_differs() {
        use remind_me_core::db::entities::Entities;
        use remind_me_core::entity::Entity;
        let db = engine();
        let store = db.store();
        let entity = Entity {
            id: "e".into(),
            name: "e".into(),
            kind: None,
            aliases: vec![],
            created_at: "2026-11-22".into(),
            updated_at: "2026-11-22".into(),
        };
        let entities = Entities::new(&store);
        entities.insert(&entity, Some("node-a")).unwrap();
        let before = Manifest::read(&store, &no_blobs()).unwrap();
        entities.upsert_synced(&entity, Some("node-b")).unwrap();
        let after = Manifest::read(&store, &no_blobs()).unwrap();
        let seen = entities.get("e").unwrap().unwrap();
        let json = |e: &Entity| serde_json::to_value(e).unwrap();
        assert_eq!(json(&seen), json(&entity), "the ports see no change");
        assert_eq!(before.differences(&after), ["entities: 1 differ"]);
    }

    /// One embedding stored at another chunk index is a different row.
    #[test]
    fn the_same_vector_at_another_chunk_index_differs() {
        use remind_me_core::db::memories::{Memories, NewMemory};
        use remind_me_core::db::vectors::Vectors;
        let manifest_with_chunk_at = |ix: usize| {
            let db = engine();
            let store = db.store();
            Memories::new(&store)
                .insert(&NewMemory::new("m", "x", "2026-11-22"))
                .unwrap();
            Vectors::new(&store).put("m", ix, &[1, 2, 3, 4]).unwrap();
            Manifest::read(&store, &no_blobs()).unwrap()
        };
        let (at_0, at_1) = (manifest_with_chunk_at(0), manifest_with_chunk_at(1));
        assert_eq!(at_1.vectors.keys().collect::<Vec<_>>(), ["m#1"]);
        assert!(at_0
            .differences(&at_1)
            .contains(&"vectors: 2 differ".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_refuses_a_symlink_before_writing_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("package_copy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("sub/file"), b"x").unwrap();
        copy_dir(&from, &root.join("ok"), &[]).unwrap();
        assert_eq!(std::fs::read(root.join("ok/sub/file")).unwrap(), b"x");
        let mode = std::fs::metadata(root.join("ok"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::os::unix::fs::symlink("/etc/hostname", from.join("sub/link")).unwrap();
        assert!(copy_dir(&from, &root.join("refused"), &[]).is_err());
        assert!(!root.join("refused").exists(), "nothing written");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
