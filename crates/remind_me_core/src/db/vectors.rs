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

use super::engine::{self, EngineLock};
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

/// The vector tables, on the engine's memories core.
pub struct Vectors<'c> {
    core: &'c EngineLock,
}

impl<'c> Vectors<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Store `embedding` as chunk `chunk_ix` of `memory_id`, replacing any
    /// chunk already there.
    pub fn put(&self, memory_id: &str, chunk_ix: usize, embedding: &[u8]) -> Result<()> {
        engine::vectors::put(&mut self.core.lock(), memory_id, chunk_ix as i64, embedding)
    }

    /// Delete every chunk of `memory_id`. Returns how many went.
    pub fn delete_for(&self, memory_id: &str) -> Result<usize> {
        engine::vectors::delete_for(&mut self.core.lock(), memory_id)
    }

    /// Delete every chunk of every memory.
    pub fn clear(&self) -> Result<()> {
        engine::vectors::clear(&mut self.core.lock())
    }

    /// How many chunks `memory_id` has.
    pub fn chunk_count(&self, memory_id: &str) -> Result<usize> {
        engine::vectors::chunk_count(&self.core.lock(), memory_id)
    }

    /// How many chunks are stored in all.
    pub fn count(&self) -> Result<usize> {
        engine::vectors::count(&self.core.lock())
    }

    /// The first stored embedding in key order, if there is one.
    pub fn any_embedding(&self) -> Result<Option<Vec<u8>>> {
        engine::vectors::any_embedding(&self.core.lock())
    }

    /// Every stored chunk, in key order.
    pub fn all(&self) -> Result<Vec<ChunkVector>> {
        engine::vectors::all(&self.core.lock())
    }

    /// The chunks of live, unsuperseded memories, of `category` when given,
    /// and only of the memories in `among` when given, in key order.
    pub fn live_chunks(
        &self,
        category: Option<&str>,
        among: Option<&[String]>,
    ) -> Result<Vec<ChunkVector>> {
        engine::vectors::live_chunks(&self.core.lock(), category, among)
    }

    /// Every live memory with no chunk vectors, oldest first.
    pub fn unembedded(&self) -> Result<Vec<Unembedded>> {
        engine::vectors::unembedded(&self.core.lock())
    }

    /// Active, unsuperseded, live memories with a first chunk vector, of
    /// `category` when given, oldest first (ties by id), at most `limit`.
    pub fn consolidation_candidates(
        &self,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationCandidate>> {
        engine::vectors::consolidation_candidates(&self.core.lock(), category, limit)
    }

    // --- embedding_meta --------------------------------------------------

    /// Every `embedding_meta` key and value, by key.
    pub fn meta(&self) -> Result<Vec<(String, String)>> {
        engine::vectors::meta(&self.core.lock())
    }

    /// Set the `embedding_meta` value under `key`, stamped `updated_at`.
    pub fn set_meta(&self, key: &str, value: &str, updated_at: &str) -> Result<()> {
        engine::vectors::set_meta(&mut self.core.lock(), key, value, updated_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn live_chunks_skip_deleted_memories_and_respect_the_narrowing() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();
        memories
            .insert(&NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..NewMemory::new("gone", "x", NOW)
            })
            .unwrap();
        let vectors = Vectors::new(&store);
        vectors.put("a", 0, &[1, 0, 0, 0]).unwrap();
        vectors.put("a", 1, &[2, 0, 0, 0]).unwrap();
        vectors.put("gone", 0, &[3, 0, 0, 0]).unwrap();

        assert_eq!(vectors.live_chunks(None, None).unwrap().len(), 2);
        assert!(vectors
            .live_chunks(None, Some(&["other".to_string()]))
            .unwrap()
            .is_empty());
        assert!(vectors.live_chunks(None, Some(&[])).unwrap().is_empty());
        assert_eq!(vectors.count().unwrap(), 3);
        assert_eq!(vectors.delete_for("a").unwrap(), 2);
        assert_eq!(vectors.chunk_count("a").unwrap(), 0);
    }

    /// Every read and write of the vector tables on a fixed corpus.
    #[test]
    fn the_vector_tables_read_back_what_was_written() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        let memory = |id: &str, created: &str| NewMemory {
            created_at: created.to_string(),
            accessed_at: Some(created.to_string()),
            tags: vec![format!("t-{id}")],
            ..NewMemory::new(id, format!("content {id}"), NOW)
        };
        memories
            .insert(&memory("b", "2026-01-02T00:00:00+00:00"))
            .unwrap();
        memories
            .insert(&memory("a", "2026-01-02T00:00:00+00:00"))
            .unwrap();
        memories
            .insert(&memory("c", "2026-01-01T00:00:00+00:00"))
            .unwrap();
        memories
            .insert(&NewMemory {
                category: "note".to_string(),
                ..memory("d", "2026-01-03T00:00:00+00:00")
            })
            .unwrap();
        memories
            .insert(&NewMemory {
                status: "archived".to_string(),
                ..memory("e", "2026-01-04T00:00:00+00:00")
            })
            .unwrap();
        memories
            .insert(&NewMemory {
                superseded_by: Some("a".to_string()),
                ..memory("f", "2026-01-05T00:00:00+00:00")
            })
            .unwrap();
        memories
            .insert(&NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..memory("g", "2026-01-06T00:00:00+00:00")
            })
            .unwrap();
        memories
            .insert(&memory("h", "2026-01-07T00:00:00+00:00"))
            .unwrap();

        let vectors = Vectors::new(&store);
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
            vectors.put(id, ix, &[byte, 0, 0, 0]).unwrap();
        }
        // A replace, not a second chunk.
        vectors.put("a", 0, &[30, 0, 0, 0]).unwrap();
        vectors.set_meta("model", "m1", NOW).unwrap();
        vectors.set_meta("dim", "4", NOW).unwrap();
        vectors.set_meta("model", "m2", NOW).unwrap();

        let ids = |chunks: Vec<ChunkVector>| -> Vec<(String, u8)> {
            chunks
                .into_iter()
                .map(|c| (c.memory_id, c.embedding[0]))
                .collect()
        };
        assert_eq!(vectors.count().unwrap(), 10, "the corpus stores its chunks");
        assert_eq!(vectors.chunk_count("a").unwrap(), 2);
        assert_eq!(vectors.any_embedding().unwrap(), Some(vec![30, 0, 0, 0]));
        assert_eq!(
            ids(vectors.all().unwrap()),
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
        let live = ids(vectors.live_chunks(None, None).unwrap());
        let live_ids: Vec<&str> = live.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            live_ids,
            ["a", "a", "b", "c", "d", "e", "h"],
            "superseded, deleted and orphaned chunks are not live"
        );
        assert_eq!(
            ids(vectors.live_chunks(Some("note"), None).unwrap()),
            [("d".to_string(), 5)]
        );
        let among = ["a".to_string(), "d".to_string(), "g".to_string()];
        assert_eq!(
            ids(vectors.live_chunks(None, Some(&among)).unwrap()),
            [("a".to_string(), 30), ("a".to_string(), 2), ("d".to_string(), 5)]
        );
        let unembedded: Vec<String> = vectors
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert!(unembedded.is_empty(), "{unembedded:?}");

        let candidates: Vec<(String, f64)> = vectors
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
        assert_eq!(vectors.consolidation_candidates(None, 2).unwrap().len(), 2);
        let notes = vectors.consolidation_candidates(Some("note"), 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tags, ["t-d"]);
        assert_eq!(notes[0].embedding, [5, 0, 0, 0]);
        assert_eq!(
            vectors.meta().unwrap(),
            [
                ("dim".to_string(), "4".to_string()),
                ("model".to_string(), "m2".to_string())
            ]
        );
        assert_eq!(vectors.delete_for("a").unwrap(), 2);
        assert_eq!(vectors.delete_for("missing").unwrap(), 0);
        let unembedded: Vec<String> = vectors
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(unembedded, ["a"]);
        vectors.clear().unwrap();
        assert_eq!(vectors.count().unwrap(), 0);
        assert_eq!(vectors.any_embedding().unwrap(), None);
    }

    #[test]
    fn a_memory_with_a_chunk_is_not_unembedded() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();
        memories.insert(&NewMemory::new("b", "y", NOW)).unwrap();
        let vectors = Vectors::new(&store);
        vectors.put("a", 0, &[1, 0, 0, 0]).unwrap();

        let missing: Vec<String> = vectors
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(missing, vec!["b"]);
    }
}
