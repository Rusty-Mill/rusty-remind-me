//! Storage for search expansion: `memory_associations` (how often two
//! memories were retrieved together) and the reads that find a result's
//! relatives through shared entities, document position and co-retrieval,
//! on the engine's memories core (`db::engine::related`,
//! `db::engine::graph`).
//!
//! Every read and write [`crate::expansion`] makes goes through here. The
//! rules stay there: the pair cap, the weight ceiling, the window size, how
//! relatives are grouped and capped.
//!
//! [`RelatedStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::related`. [`Related`] is what callers take.

use super::engine::EngineLock;
use super::{Result, Store};

/// A live memory another one points at, with the fields expansion shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Relative {
    pub id: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

/// A relative found through an entity, one row per entity it shares.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityRelative {
    pub memory: Relative,
    pub entity_name: String,
}

/// A chunk of a document, with its position.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentChunk {
    pub memory: Relative,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
}

/// A relative found through co-retrieval, with the pair's weight.
#[derive(Debug, Clone, PartialEq)]
pub struct CoRetrieved {
    pub memory: Relative,
    pub weight: i64,
}

/// The expansion table and reads, as a backend stores them (ADR-0035).
/// [`conformance`] holds the behaviour every implementation must show.
pub trait RelatedStore {
    /// Record that `a` and `b` were retrieved together at `now`: a new pair
    /// starts at weight 1, and a known one gains 1 up to `max_weight`. The
    /// caller orders the pair, so `(a, b)` and `(b, a)` are one row.
    fn bump_pair(&mut self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()>;

    /// Live memories outside `seed_ids` that mention an entity a seed
    /// mentions, newest first (then id, then entity name), one row per
    /// shared entity. A link whose memory or entity is not stored is
    /// skipped; no seeds find nothing.
    fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>>;

    /// Remove every pair `memory_id` is part of: part of deleting a memory.
    fn unlink_memory(&mut self, memory_id: &str) -> Result<()>;

    /// Live chunks of `doc_id` from position `from` to `to` inclusive, in
    /// order (ties by id).
    fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>>;

    /// Live memories paired with any of `seed_ids`, strongest pair first,
    /// then newest, then id. A pair is read from either side, since it is
    /// stored once; no seeds find nothing.
    fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>>;
}

/// The expansion table and reads of a [`Store`], one locked call at a time.
pub struct Related<'c> {
    core: &'c EngineLock,
}

impl<'c> Related<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// See [`RelatedStore::bump_pair`].
    pub fn bump_pair(&self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()> {
        RelatedStore::bump_pair(&mut *self.core.lock(), a, b, now, max_weight)
    }

    /// See [`RelatedStore::via_entities`].
    pub fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        RelatedStore::via_entities(&*self.core.lock(), seed_ids)
    }

    /// See [`RelatedStore::unlink_memory`].
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        RelatedStore::unlink_memory(&mut *self.core.lock(), memory_id)
    }

    /// See [`RelatedStore::document_window`].
    pub fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>> {
        RelatedStore::document_window(&*self.core.lock(), doc_id, from, to)
    }

    /// See [`RelatedStore::co_retrieved`].
    pub fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        RelatedStore::co_retrieved(&*self.core.lock(), seed_ids)
    }
}

/// The behaviour every [`RelatedStore`] must show, run against each backend
/// by [`related_conformance`] (ADR-0035).
///
/// Relatives are memories found through entities, so a backend under test
/// implements [`MemoryStore`] and [`EntityStore`] too.
#[cfg(test)]
pub(crate) mod conformance {
    use super::RelatedStore;
    use crate::db::derived::Origin;
    use crate::db::entities::EntityStore;
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::entity::Entity;

    const NOW: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Memories, entities, links and pairs, with `z` deleted and `d3`
    /// superseded.
    fn seed<S: RelatedStore + MemoryStore + EntityStore>(store: &mut S) {
        for (id, at, doc, chunk) in [
            ("a", NOW, None, None),
            ("b", T2, None, None),
            ("c", T2, Some("d"), Some(0)),
            ("d1", NOW, Some("d"), Some(1)),
            ("d2", NOW, Some("d"), Some(2)),
            ("d3", T2, Some("d"), Some(3)),
            ("z", T2, None, None),
        ] {
            MemoryStore::insert(
                store,
                &NewMemory {
                    doc_id: doc.map(String::from),
                    chunk_index: chunk,
                    ..NewMemory::new(id, format!("{id} content"), at)
                },
            )
            .unwrap();
        }
        MemoryStore::set_superseded_by(store, "d3", "a", None).unwrap();
        for id in ["e1", "e2"] {
            let entity = Entity {
                id: id.into(),
                name: id.to_uppercase(),
                kind: None,
                aliases: Vec::new(),
                created_at: NOW.into(),
                updated_at: NOW.into(),
            };
            EntityStore::insert(store, &entity, None).unwrap();
        }
        for (m, e) in [
            ("a", "e1"),
            ("b", "e1"),
            ("c", "e1"),
            ("a", "e2"),
            ("c", "e2"),
            ("z", "e2"),
            ("d1", "e1"),
            ("a", "ghost"),
        ] {
            EntityStore::link(store, m, e, NOW, Origin::Local).unwrap();
        }
        for (a, b, times) in [("a", "b", 2), ("a", "c", 5), ("b", "c", 1), ("a", "z", 1)] {
            for _ in 0..times {
                RelatedStore::bump_pair(store, a, b, NOW, 3).unwrap();
            }
        }
        MemoryStore::delete_live(store, "z", Some(T2)).unwrap();
    }

    pub(crate) fn via_entities_is_newest_first_one_row_per_entity<
        S: RelatedStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        seed(store);
        let via: Vec<(String, String)> = store
            .via_entities(&ids(&["a"]))
            .unwrap()
            .into_iter()
            .map(|r| (r.memory.id, r.entity_name))
            .collect();
        assert_eq!(
            via,
            [
                ("b".to_string(), "E1".to_string()),
                ("c".to_string(), "E1".to_string()),
                ("c".to_string(), "E2".to_string()),
                ("d1".to_string(), "E1".to_string()),
            ],
            "newest first, one row per shared entity, no deleted z, no unknown entity"
        );
        assert!(store.via_entities(&[]).unwrap().is_empty());
    }

    pub(crate) fn document_window_is_live_chunks_in_order<
        S: RelatedStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        seed(store);
        let window: Vec<(String, Option<i64>)> = store
            .document_window("d", 1, 3)
            .unwrap()
            .into_iter()
            .map(|c| (c.memory.id, c.chunk_index))
            .collect();
        assert_eq!(
            window,
            [("d1".to_string(), Some(1)), ("d2".to_string(), Some(2))],
            "the superseded d3 is out"
        );
        assert!(store.document_window("other", 0, 9).unwrap().is_empty());
    }

    pub(crate) fn co_retrieved_is_capped_strongest_first_and_live<
        S: RelatedStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        seed(store);
        let co: Vec<(String, i64)> = store
            .co_retrieved(&ids(&["a"]))
            .unwrap()
            .into_iter()
            .map(|c| (c.memory.id, c.weight))
            .collect();
        assert_eq!(
            co,
            [("c".to_string(), 3), ("b".to_string(), 2)],
            "capped at 3, strongest first, deleted z gone"
        );
        assert_eq!(store.co_retrieved(&ids(&["a", "c"])).unwrap().len(), 4);
        assert!(store.co_retrieved(&[]).unwrap().is_empty());
    }

    pub(crate) fn a_pair_is_read_from_either_side<S: RelatedStore + MemoryStore + EntityStore>(
        store: &mut S,
    ) {
        seed(store);
        let from_b: Vec<String> = store
            .co_retrieved(&ids(&["b"]))
            .unwrap()
            .into_iter()
            .map(|c| c.memory.id)
            .collect();
        assert_eq!(from_b, ["a", "c"]);
    }

    pub(crate) fn unlink_memory_drops_its_pairs<S: RelatedStore + MemoryStore + EntityStore>(
        store: &mut S,
    ) {
        seed(store);
        RelatedStore::unlink_memory(store, "c").unwrap();
        let co: Vec<String> = store
            .co_retrieved(&ids(&["a"]))
            .unwrap()
            .into_iter()
            .map(|c| c.memory.id)
            .collect();
        assert_eq!(co, ["b"]);
        RelatedStore::unlink_memory(store, "c").unwrap();
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`. A backend's adapter invokes it in its own test module.
#[cfg(test)]
macro_rules! related_conformance {
    ($make:expr) => {
        use $crate::db::related::conformance;

        #[test]
        fn via_entities_is_newest_first_one_row_per_entity() {
            conformance::via_entities_is_newest_first_one_row_per_entity(&mut $make);
        }
        #[test]
        fn document_window_is_live_chunks_in_order() {
            conformance::document_window_is_live_chunks_in_order(&mut $make);
        }
        #[test]
        fn co_retrieved_is_capped_strongest_first_and_live() {
            conformance::co_retrieved_is_capped_strongest_first_and_live(&mut $make);
        }
        #[test]
        fn a_pair_is_read_from_either_side() {
            conformance::a_pair_is_read_from_either_side(&mut $make);
        }
        #[test]
        fn unlink_memory_drops_its_pairs() {
            conformance::unlink_memory_drops_its_pairs(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use related_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        for id in ["a", "b"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
        }
        let related = Related::new(&store);
        related.bump_pair("a", "b", NOW, 3).unwrap();
        let co = related.co_retrieved(&["a".to_string()]).unwrap();
        assert_eq!((co[0].memory.id.as_str(), co[0].weight), ("b", 1));
        assert!(related.co_retrieved(&[]).unwrap().is_empty());
        assert!(related.via_entities(&[]).unwrap().is_empty());
        assert!(related.document_window("d", 0, 1).unwrap().is_empty());
        related.unlink_memory("a").unwrap();
        assert!(related.co_retrieved(&["a".to_string()]).unwrap().is_empty());
    }
}
