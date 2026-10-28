//! Storage for search expansion: `memory_associations` (how often two
//! memories were retrieved together) and the reads that find a result's
//! relatives through shared entities, document position and co-retrieval,
//! on the engine's memories core (`db::engine::related`,
//! `db::engine::graph`).
//!
//! Every read and write [`crate::expansion`] makes goes through here. The
//! rules stay there: the pair cap, the weight ceiling, the window size, how
//! relatives are grouped and capped.

use super::engine::{self, EngineLock};
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

/// The expansion table and reads, on the engine's memories core.
pub struct Related<'c> {
    core: &'c EngineLock,
}

impl<'c> Related<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Record that `a` and `b` were retrieved together at `now`: a new pair
    /// starts at weight 1, and a known one gains 1 up to `max_weight`. The
    /// caller orders the pair, so `(a, b)` and `(b, a)` are one row.
    pub fn bump_pair(&self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()> {
        engine::related::bump_pair(&mut self.core.lock(), a, b, now, max_weight)
    }

    /// Live memories outside `seed_ids` that mention an entity a seed
    /// mentions, newest first, one row per shared entity. A link whose
    /// memory or entity is not stored here is skipped.
    pub fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        engine::graph::via_entities(&self.core.lock(), seed_ids)
    }

    /// Remove every pair `memory_id` is part of: part of deleting a memory.
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        engine::related::unlink_memory(&mut self.core.lock(), memory_id)
    }

    /// Live chunks of `doc_id` from position `from` to `to`, in order (ties by
    /// id).
    pub fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>> {
        engine::related::document_window(&self.core.lock(), doc_id, from, to)
    }

    /// Live memories paired with any of `seed_ids`, strongest pair first,
    /// then newest, then id. A pair is read from either side, since it is
    /// stored once.
    pub fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        engine::related::co_retrieved(&self.core.lock(), seed_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Associations, links and chunks, and every expansion read and sync
    /// feed page over them.
    #[test]
    fn expansion_reads_and_feeds_follow_the_graph() {
        use crate::db::derived::Origin;
        use crate::db::entities::{Entities, RelationRow};
        use crate::db::sync_feed::SyncFeed;
        use crate::entity::Entity;
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        for (id, at, doc, chunk, node) in [
            ("a", NOW, None, None, Some("n1")),
            ("b", T2, None, None, Some("n2")),
            ("c", T2, Some("d"), Some(0), None),
            ("d1", NOW, Some("d"), Some(1), Some("n1")),
            ("d2", NOW, Some("d"), Some(2), None),
            ("d3", T2, Some("d"), Some(3), None),
            ("z", T2, None, None, None),
        ] {
            memories
                .insert(&NewMemory {
                    doc_id: doc.map(String::from),
                    chunk_index: chunk,
                    node_id: node.map(String::from),
                    tags: vec!["t".into()],
                    metadata: serde_json::json!({"k": id}),
                    ..NewMemory::new(id, format!("{id} content"), at)
                })
                .unwrap();
        }
        memories.set_superseded_by("d3", "a", None).unwrap();
        let entities = Entities::new(&store);
        for (id, node) in [("e1", Some("n1")), ("e2", None)] {
            entities
                .insert(
                    &Entity {
                        id: id.into(),
                        name: id.to_uppercase(),
                        kind: None,
                        aliases: vec!["x".into()],
                        created_at: NOW.into(),
                        updated_at: NOW.into(),
                    },
                    node,
                )
                .unwrap();
        }
        for (m, e, at) in [
            ("a", "e1", NOW),
            ("b", "e1", NOW),
            ("c", "e1", T2),
            ("a", "e2", T2),
            ("c", "e2", NOW),
            ("z", "e2", NOW),
            ("d1", "e1", NOW),
        ] {
            entities.link(m, e, at, Origin::Local).unwrap();
        }
        entities
            .insert_relation_or_ignore(
                &RelationRow {
                    id: "r1",
                    subject_entity_id: "e1",
                    relation: "near",
                    object_entity_id: "e2",
                    created_at: NOW,
                    updated_at: NOW,
                    node_id: Some("n1"),
                },
                Origin::Local,
            )
            .unwrap();
        let related = Related::new(&store);
        for (a, b, times) in [("a", "b", 2), ("a", "c", 5), ("b", "c", 1), ("a", "z", 1)] {
            for _ in 0..times {
                related.bump_pair(a, b, NOW, 3).unwrap();
            }
        }
        memories.delete_live("z", Some(T2)).unwrap();

        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let via: Vec<(String, String)> = related
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
            "newest first, one row per shared entity, no deleted z"
        );
        assert!(
            related.via_entities(&ids(&["a", "c"])).unwrap().len() > 2,
            "a second seed widens the set"
        );
        let window: Vec<(String, Option<i64>)> = related
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
        let co: Vec<(String, i64)> = related
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
        assert_eq!(related.co_retrieved(&ids(&["a", "c"])).unwrap().len(), 4);
        related.unlink_memory("c").unwrap();
        let co: Vec<String> = related
            .co_retrieved(&ids(&["a"]))
            .unwrap()
            .into_iter()
            .map(|c| c.memory.id)
            .collect();
        assert_eq!(co, ["b"]);

        let feed = SyncFeed::new(&store);
        let ids_of = |page: Vec<serde_json::Value>| -> Vec<String> {
            page.into_iter()
                .map(|v| v["id"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(
            ids_of(feed.memories_after("", "", None, 100).unwrap()),
            ["a", "d1", "d2", "b", "c", "d3", "z"],
            "by (updated_at, id)"
        );
        assert_eq!(
            ids_of(feed.memories_after(NOW, "a", None, 3).unwrap()),
            ["d1", "d2", "b"]
        );
        assert_eq!(
            ids_of(feed.memories_after(NOW, "d2", Some("n1"), 100).unwrap()),
            ["b", "c", "d3", "z"]
        );
        assert_eq!(
            ids_of(feed.memories_after("", "", Some("n1"), 100).unwrap()),
            ["d2", "b", "c", "d3", "z"],
            "n1's own a and d1 are excluded"
        );
        assert_eq!(
            ids_of(feed.entities_after("", "", Some("n1"), 100).unwrap()),
            ["e2"]
        );
        let links = feed.links_after("", "", 100).unwrap();
        assert_eq!(links.len(), 7);
        assert_eq!(links[0]["record_type"], "memory_entity");
        assert_eq!(
            ids_of(feed.links_after(NOW, "b|e1", 2).unwrap()),
            ["c|e2", "d1|e1"]
        );
        let relations = feed.relations_after("", "", 10).unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["node_id"], "n1");
        assert_eq!(
            (
                feed.entity_count().unwrap(),
                feed.link_count().unwrap(),
                feed.relation_count().unwrap()
            ),
            (2, 7, 1)
        );
    }

    #[test]
    fn a_pair_is_read_from_either_side_and_its_weight_is_capped() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        for id in ["a", "b"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
        }
        let related = Related::new(&store);
        for _ in 0..5 {
            related.bump_pair("a", "b", NOW, 3).unwrap();
        }

        let from_a = related.co_retrieved(&["a".to_string()]).unwrap();
        let from_b = related.co_retrieved(&["b".to_string()]).unwrap();
        assert_eq!((from_a[0].memory.id.as_str(), from_a[0].weight), ("b", 3));
        assert_eq!(from_b[0].memory.id, "a");
        assert!(related.co_retrieved(&[]).unwrap().is_empty());
    }
}
