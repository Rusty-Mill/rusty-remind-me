//! Storage for embeddings: `vec_chunks`, one row per chunk of a memory,
//! keyed `(memory_id, chunk_ix)` and holding the vector as little-endian
//! f32 bytes, plus `embedding_meta`, which records the model that wrote
//! them. Both on the engine's memories core (`db::engine::vectors`).
//!
//! Chunks were keyed on `memories.rowid` up to schema v29; they are keyed
//! on the memory id now, so nothing depends on a row number (ADR-0023 §4).
//! Every read and write [`crate::vectors`], [`crate::ann_index`] and
//! [`crate::consolidation`] make goes through here. The rules stay there:
//! chunking, scoring, the ANN index's staleness test, and the model-change
//! clear.
//!
//! [`VectorStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::vectors`. [`Vectors`] is what callers take.

use super::backend::BackendLock;
use super::{Result, Store};

/// One stored chunk vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkVector {
    pub memory_id: String,
    pub embedding: Vec<u8>,
}

/// A memory with no chunk vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct Unembedded {
    pub id: String,
    pub content: String,
}

/// A consolidation candidate: an active memory and its first chunk vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationCandidate {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub access_count: i64,
    pub accessed_at: String,
    pub tags: Vec<String>,
    pub decay_rate: f64,
    pub base_weight: f64,
    pub embedding: Vec<u8>,
}

/// The vector tables, as a backend stores them (ADR-0035).
/// [`conformance`] holds the behaviour every implementation must show.
pub trait VectorStore {
    /// Store `embedding` as chunk `chunk_ix` of `memory_id`, replacing any
    /// chunk already there.
    fn put(&mut self, memory_id: &str, chunk_ix: usize, embedding: &[u8]) -> Result<()>;

    /// Delete every chunk of `memory_id`. Returns how many went.
    fn delete_for(&mut self, memory_id: &str) -> Result<usize>;

    /// Delete every chunk of every memory.
    fn clear(&mut self) -> Result<()>;

    /// How many chunks `memory_id` has.
    fn chunk_count(&self, memory_id: &str) -> Result<usize>;

    /// The chunks of `memory_id`, by chunk index.
    fn chunks_for(&self, memory_id: &str) -> Result<Vec<ChunkVector>>;

    /// How many chunks are stored in all.
    fn count(&self) -> Result<usize>;

    /// The first stored embedding in key order, if there is one.
    fn any_embedding(&self) -> Result<Option<Vec<u8>>>;

    /// Every stored chunk, in key order: by memory id, then chunk index.
    fn all(&self) -> Result<Vec<ChunkVector>>;

    /// The chunks of live, unsuperseded memories, of `category` when given,
    /// and only of the memories in `among` when given, in key order.
    fn live_chunks(
        &self,
        category: Option<&str>,
        among: Option<&[String]>,
    ) -> Result<Vec<ChunkVector>>;

    /// Every live memory with no chunk vectors, oldest first.
    fn unembedded(&self) -> Result<Vec<Unembedded>>;

    /// Active, unsuperseded, live memories with a first chunk vector, of
    /// `category` when given, oldest first (ties by id), at most `limit`.
    fn consolidation_candidates(
        &self,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationCandidate>>;

    /// Every `embedding_meta` key and value, by key.
    fn meta(&self) -> Result<Vec<(String, String)>>;

    /// Set the `embedding_meta` value under `key`, stamped `updated_at`.
    fn set_meta(&mut self, key: &str, value: &str, updated_at: &str) -> Result<()>;
}

/// The vector tables of a [`Store`], one locked call at a time.
pub struct Vectors<'c> {
    backend: &'c BackendLock,
}

impl<'c> Vectors<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`VectorStore::put`].
    pub fn put(&self, memory_id: &str, chunk_ix: usize, embedding: &[u8]) -> Result<()> {
        VectorStore::put(&mut *self.backend.lock()?, memory_id, chunk_ix, embedding)
    }

    /// See [`VectorStore::delete_for`].
    pub fn delete_for(&self, memory_id: &str) -> Result<usize> {
        VectorStore::delete_for(&mut *self.backend.lock()?, memory_id)
    }

    /// See [`VectorStore::clear`].
    pub fn clear(&self) -> Result<()> {
        VectorStore::clear(&mut *self.backend.lock()?)
    }

    /// See [`VectorStore::chunk_count`].
    pub fn chunk_count(&self, memory_id: &str) -> Result<usize> {
        VectorStore::chunk_count(&*self.backend.lock()?, memory_id)
    }

    /// See [`VectorStore::chunks_for`].
    pub fn chunks_for(&self, memory_id: &str) -> Result<Vec<ChunkVector>> {
        VectorStore::chunks_for(&*self.backend.lock()?, memory_id)
    }

    /// See [`VectorStore::count`].
    pub fn count(&self) -> Result<usize> {
        VectorStore::count(&*self.backend.lock()?)
    }

    /// See [`VectorStore::any_embedding`].
    pub fn any_embedding(&self) -> Result<Option<Vec<u8>>> {
        VectorStore::any_embedding(&*self.backend.lock()?)
    }

    /// See [`VectorStore::all`].
    pub fn all(&self) -> Result<Vec<ChunkVector>> {
        VectorStore::all(&*self.backend.lock()?)
    }

    /// See [`VectorStore::live_chunks`].
    pub fn live_chunks(
        &self,
        category: Option<&str>,
        among: Option<&[String]>,
    ) -> Result<Vec<ChunkVector>> {
        VectorStore::live_chunks(&*self.backend.lock()?, category, among)
    }

    /// See [`VectorStore::unembedded`].
    pub fn unembedded(&self) -> Result<Vec<Unembedded>> {
        VectorStore::unembedded(&*self.backend.lock()?)
    }

    /// See [`VectorStore::consolidation_candidates`].
    pub fn consolidation_candidates(
        &self,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationCandidate>> {
        VectorStore::consolidation_candidates(&*self.backend.lock()?, category, limit)
    }

    /// See [`VectorStore::meta`].
    pub fn meta(&self) -> Result<Vec<(String, String)>> {
        VectorStore::meta(&*self.backend.lock()?)
    }

    /// See [`VectorStore::set_meta`].
    pub fn set_meta(&self, key: &str, value: &str, updated_at: &str) -> Result<()> {
        VectorStore::set_meta(&mut *self.backend.lock()?, key, value, updated_at)
    }
}

/// The behaviour every [`VectorStore`] must show, run against each backend
/// by [`vector_conformance`] (ADR-0035).
///
/// Liveness and candidates are read from memories, so a backend under test
/// implements [`MemoryStore`] too.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{ChunkVector, VectorStore};
    use crate::db::memories::{MemoryStore, NewMemory};

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    pub(crate) fn live_chunks_skip_deleted_memories_and_respect_the_narrowing<
        S: VectorStore + MemoryStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(store, &NewMemory::new("a", "x", NOW)).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..NewMemory::new("gone", "x", NOW)
            },
        )
        .unwrap();
        store.put("a", 0, &[1, 0, 0, 0]).unwrap();
        store.put("a", 1, &[2, 0, 0, 0]).unwrap();
        store.put("gone", 0, &[3, 0, 0, 0]).unwrap();

        assert_eq!(store.live_chunks(None, None).unwrap().len(), 2);
        assert!(store
            .live_chunks(None, Some(&["other".to_string()]))
            .unwrap()
            .is_empty());
        assert!(store.live_chunks(None, Some(&[])).unwrap().is_empty());
        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(store.delete_for("a").unwrap(), 2);
        assert_eq!(store.chunk_count("a").unwrap(), 0);
    }

    /// Every read and write of the vector tables on a fixed corpus.
    pub(crate) fn the_vector_tables_read_back_what_was_written<S: VectorStore + MemoryStore>(
        store: &mut S,
    ) {
        let memory = |id: &str, created: &str| NewMemory {
            created_at: created.to_string(),
            accessed_at: Some(created.to_string()),
            tags: vec![format!("t-{id}")],
            ..NewMemory::new(id, format!("content {id}"), NOW)
        };
        MemoryStore::insert(store, &memory("b", "2026-01-02T00:00:00+00:00")).unwrap();
        MemoryStore::insert(store, &memory("a", "2026-01-02T00:00:00+00:00")).unwrap();
        MemoryStore::insert(store, &memory("c", "2026-01-01T00:00:00+00:00")).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                category: "note".to_string(),
                ..memory("d", "2026-01-03T00:00:00+00:00")
            },
        )
        .unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                status: "archived".to_string(),
                ..memory("e", "2026-01-04T00:00:00+00:00")
            },
        )
        .unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                superseded_by: Some("a".to_string()),
                ..memory("f", "2026-01-05T00:00:00+00:00")
            },
        )
        .unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..memory("g", "2026-01-06T00:00:00+00:00")
            },
        )
        .unwrap();
        MemoryStore::insert(store, &memory("h", "2026-01-07T00:00:00+00:00")).unwrap();

        for (id, ix, byte) in [
            ("b", 0, 1),
            ("a", 1, 2),
            ("a", 0, 3),
            ("c", 0, 4),
            ("d", 0, 5),
            ("e", 0, 6),
            ("f", 0, 7),
            ("g", 0, 8),
            ("orphan", 0, 9),
            ("h", 1, 10),
        ] {
            store.put(id, ix, &[byte, 0, 0, 0]).unwrap();
        }
        // A replace, not a second chunk.
        store.put("a", 0, &[30, 0, 0, 0]).unwrap();
        store.set_meta("model", "m1", NOW).unwrap();
        store.set_meta("dim", "4", NOW).unwrap();
        store.set_meta("model", "m2", NOW).unwrap();

        let ids = |chunks: Vec<ChunkVector>| -> Vec<(String, u8)> {
            chunks
                .into_iter()
                .map(|c| (c.memory_id, c.embedding[0]))
                .collect()
        };
        assert_eq!(store.count().unwrap(), 10, "the corpus stores its chunks");
        assert_eq!(store.chunk_count("a").unwrap(), 2);
        assert_eq!(store.any_embedding().unwrap(), Some(vec![30, 0, 0, 0]));
        assert_eq!(
            ids(store.all().unwrap()),
            [
                ("a".to_string(), 30),
                ("a".to_string(), 2),
                ("b".to_string(), 1),
                ("c".to_string(), 4),
                ("d".to_string(), 5),
                ("e".to_string(), 6),
                ("f".to_string(), 7),
                ("g".to_string(), 8),
                ("h".to_string(), 10),
                ("orphan".to_string(), 9),
            ],
            "key order"
        );
        let live = ids(store.live_chunks(None, None).unwrap());
        let live_ids: Vec<&str> = live.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            live_ids,
            ["a", "a", "b", "c", "d", "e", "h"],
            "superseded, deleted and orphaned chunks are not live"
        );
        assert_eq!(
            ids(store.live_chunks(Some("note"), None).unwrap()),
            [("d".to_string(), 5)]
        );
        let among = ["a".to_string(), "d".to_string(), "g".to_string()];
        assert_eq!(
            ids(store.live_chunks(None, Some(&among)).unwrap()),
            [
                ("a".to_string(), 30),
                ("a".to_string(), 2),
                ("d".to_string(), 5)
            ]
        );
        let unembedded: Vec<String> = store
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert!(unembedded.is_empty(), "{unembedded:?}");

        let candidates: Vec<(String, f64)> = store
            .consolidation_candidates(None, 10)
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.vitality))
            .collect();
        assert_eq!(
            candidates,
            [
                ("c".to_string(), 1.0),
                ("a".to_string(), 1.0),
                ("b".to_string(), 1.0),
                ("d".to_string(), 1.0)
            ],
            "active, live, unsuperseded, with a first chunk; oldest first"
        );
        assert_eq!(store.consolidation_candidates(None, 2).unwrap().len(), 2);
        let notes = store.consolidation_candidates(Some("note"), 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tags, ["t-d"]);
        assert_eq!(notes[0].embedding, [5, 0, 0, 0]);
        assert_eq!(
            store.meta().unwrap(),
            [
                ("dim".to_string(), "4".to_string()),
                ("model".to_string(), "m2".to_string())
            ]
        );
        assert_eq!(store.delete_for("a").unwrap(), 2);
        assert_eq!(store.delete_for("missing").unwrap(), 0);
        let unembedded: Vec<String> = store
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(unembedded, ["a"]);
        store.clear().unwrap();
        assert_eq!(store.count().unwrap(), 0);
        assert_eq!(store.any_embedding().unwrap(), None);
    }

    pub(crate) fn a_memory_with_a_chunk_is_not_unembedded<S: VectorStore + MemoryStore>(
        store: &mut S,
    ) {
        MemoryStore::insert(store, &NewMemory::new("a", "x", NOW)).unwrap();
        MemoryStore::insert(store, &NewMemory::new("b", "y", NOW)).unwrap();
        store.put("a", 0, &[1, 0, 0, 0]).unwrap();

        let missing: Vec<String> = store
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(missing, vec!["b"]);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`. A backend's adapter invokes it in its own test module.
#[cfg(test)]
macro_rules! vector_conformance {
    ($make:expr) => {
        use $crate::db::vectors::conformance;

        #[test]
        fn live_chunks_skip_deleted_memories_and_respect_the_narrowing() {
            conformance::live_chunks_skip_deleted_memories_and_respect_the_narrowing(&mut $make);
        }
        #[test]
        fn the_vector_tables_read_back_what_was_written() {
            conformance::the_vector_tables_read_back_what_was_written(&mut $make);
        }
        #[test]
        fn a_memory_with_a_chunk_is_not_unembedded() {
            conformance::a_memory_with_a_chunk_is_not_unembedded(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use vector_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let now = "2026-09-26T00:00:00+00:00";
        Memories::new(&store)
            .insert(&NewMemory {
                accessed_at: Some(now.to_string()),
                ..NewMemory::new("a", "x", now)
            })
            .unwrap();
        let vectors = Vectors::new(&store);
        vectors.put("a", 0, &[1, 0, 0, 0]).unwrap();
        vectors
            .set_meta("model", "m", "2026-09-26T00:00:00+00:00")
            .unwrap();
        assert_eq!(vectors.chunks_for("a").unwrap().len(), 1);
        assert_eq!(vectors.live_chunks(None, None).unwrap().len(), 1);
        assert_eq!(vectors.consolidation_candidates(None, 5).unwrap().len(), 1);
        assert_eq!(vectors.meta().unwrap().len(), 1);
        vectors.clear().unwrap();
        assert_eq!(vectors.count().unwrap(), 0);
    }
}
