//! Storage for search expansion: `memory_associations` (how often two
//! memories were retrieved together) and the reads that find a result's
//! relatives through shared entities, document position and co-retrieval.
//!
//! ADR-0023 phase 1, step 8. Every statement [`crate::expansion`] ran lives
//! here. The rules stay there: the pair cap, the weight ceiling, the window
//! size, how relatives are grouped and capped.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};

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

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// `ids` twice over, for a query that binds the list on both sides.
fn twice(ids: &[String]) -> Vec<SqlValue> {
    ids.iter()
        .chain(ids.iter())
        .map(|id| SqlValue::Text(id.clone()))
        .collect()
}

/// The expansion tables and reads, over one connection, or on the engine's
/// memories core when the store's tables hold it (`db::engine::related`).
pub struct Related<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> Related<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// Record that `a` and `b` were retrieved together at `now`: a new pair
    /// starts at weight 1, and a known one gains 1 up to `max_weight`. The
    /// caller orders the pair, so `(a, b)` and `(b, a)` are one row.
    pub fn bump_pair(&self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::related::bump_pair(&mut core.lock(), a, b, now, max_weight);
        }
        self.conn.execute(
            "INSERT INTO memory_associations (memory_id_a, memory_id_b, weight, updated_at)
             VALUES (?, ?, 1, ?)
             ON CONFLICT(memory_id_a, memory_id_b) DO UPDATE SET
                 weight = MIN(weight + 1, ?),
                 updated_at = excluded.updated_at",
            params![a, b, now, max_weight],
        )?;
        Ok(())
    }

    /// Live memories outside `seed_ids` that mention an entity a seed
    /// mentions, newest first, one row per shared entity. A link whose
    /// memory or entity is not stored here is skipped.
    pub fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::via_entities(&core.lock(), seed_ids);
        }
        let ph = placeholders(seed_ids.len());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.category, m.created_at, e.name AS entity_name
               FROM memory_entities seed
               JOIN memory_entities nbr ON nbr.entity_id = seed.entity_id
               JOIN entities e ON e.id = seed.entity_id
               JOIN memories m ON m.id = nbr.memory_id
              WHERE seed.memory_id IN ({ph})
                AND nbr.memory_id NOT IN ({ph})
                AND m.superseded_by IS NULL
                AND m.deleted_at IS NULL
              ORDER BY m.created_at DESC, m.id, e.name"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(twice(seed_ids)), |row| {
                Ok(EntityRelative {
                    memory: Relative {
                        id: row.get("id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    entity_name: row.get("entity_name")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Remove every pair `memory_id` is part of: part of deleting a memory,
    /// since the table has no foreign key to cascade.
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::related::unlink_memory(&mut core.lock(), memory_id);
        }
        self.conn.execute(
            "DELETE FROM memory_associations WHERE memory_id_a = ? OR memory_id_b = ?",
            params![memory_id, memory_id],
        )?;
        Ok(())
    }

    /// Live chunks of `doc_id` from position `from` to `to`, in order (ties by
    /// id).
    pub fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::related::document_window(&core.lock(), doc_id, from, to);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, content, category, created_at, doc_id, chunk_index
               FROM memories
              WHERE doc_id = ?
                AND chunk_index BETWEEN ? AND ?
                AND superseded_by IS NULL
                AND deleted_at IS NULL
              ORDER BY chunk_index, id",
        )?;
        let rows = stmt
            .query_map(params![doc_id, from, to], |row| {
                Ok(DocumentChunk {
                    memory: Relative {
                        id: row.get("id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    doc_id: row.get("doc_id")?,
                    chunk_index: row.get("chunk_index")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Live memories paired with any of `seed_ids`, strongest pair first,
    /// then newest, then id. A pair is read from either side, since it is
    /// stored once.
    pub fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::related::co_retrieved(&core.lock(), seed_ids);
        }
        let ph = placeholders(seed_ids.len());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT assoc.other_id, assoc.weight, m.content, m.category, m.created_at
               FROM (
                    SELECT memory_id_b AS other_id, weight FROM memory_associations
                     WHERE memory_id_a IN ({ph})
                    UNION ALL
                    SELECT memory_id_a AS other_id, weight FROM memory_associations
                     WHERE memory_id_b IN ({ph})
               ) assoc
               JOIN memories m ON m.id = assoc.other_id
              WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL
              ORDER BY assoc.weight DESC, m.created_at DESC, m.id"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(twice(seed_ids)), |row| {
                Ok(CoRetrieved {
                    memory: Relative {
                        id: row.get("other_id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    weight: row.get("weight")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Associations, links and chunks on `db`, and every expansion read and
    /// sync feed page over them.
    fn exercise(db: &Database) -> Vec<String> {
        use crate::db::derived::Origin;
        use crate::db::entities::{Entities, RelationRow};
        use crate::db::sync_feed::SyncFeed;
        use crate::entity::Entity;
        const T2: &str = "2026-09-27T00:00:00+00:00";
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
        let mut seen = vec![
            format!("{:?}", related.via_entities(&ids(&["a"])).unwrap()),
            format!("{:?}", related.via_entities(&ids(&["a", "c"])).unwrap()),
            format!("{:?}", related.document_window("d", 1, 3).unwrap()),
            format!("{:?}", related.co_retrieved(&ids(&["a"])).unwrap()),
            format!("{:?}", related.co_retrieved(&ids(&["a", "c"])).unwrap()),
        ];
        related.unlink_memory("c").unwrap();
        seen.push(format!("{:?}", related.co_retrieved(&ids(&["a"])).unwrap()));

        let feed = SyncFeed::new(&store);
        for (since, since_id, exclude, limit) in [
            ("", "", None, 100),
            (NOW, "a", None, 3),
            (NOW, "d2", Some("n1"), 100),
        ] {
            seen.push(format!(
                "{:?}",
                feed.memories_after(since, since_id, exclude, limit)
                    .unwrap()
            ));
            seen.push(format!(
                "{:?}",
                feed.entities_after(since, since_id, exclude, limit)
                    .unwrap()
            ));
        }
        seen.push(format!("{:?}", feed.links_after("", "", 100).unwrap()));
        seen.push(format!("{:?}", feed.links_after(NOW, "b|e1", 2).unwrap()));
        seen.push(format!("{:?}", feed.relations_after("", "", 10).unwrap()));
        seen.push(format!(
            "{} {} {}",
            feed.entity_count().unwrap(),
            feed.link_count().unwrap(),
            feed.relation_count().unwrap()
        ));
        seen
    }

    #[test]
    fn the_engine_core_expands_and_feeds_as_sqlite_does() {
        let mut observed = Vec::new();
        crate::db::on_each_core_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert!(
            sqlite[1].matches("EntityRelative").count() > 2,
            "{}",
            sqlite[1]
        );
        for other in &observed[1..] {
            for (theirs, ours) in other.iter().zip(sqlite) {
                assert_eq!(theirs, ours);
            }
            assert_eq!(other.len(), sqlite.len());
        }
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
