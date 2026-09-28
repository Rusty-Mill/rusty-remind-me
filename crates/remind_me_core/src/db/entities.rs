//! Storage for the knowledge graph: `entities`, `entity_relations` and the
//! `memory_entities` mention links.
//!
//! ADR-0023 phase 1, step 2. Every statement [`crate::entity`] and
//! [`crate::sync::graph`] ran lives here. The rules stay with them: how a
//! name normalises into an id, how aliases merge, which kind wins, how a
//! traversal walks, and how sync resolves a conflict.
//!
//! With the `engine-store` feature, a store whose tables hold the memories
//! core keeps the graph there (`db::engine::graph`, ADR-0023 core PR 3a).

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
use crate::db::derived::{queue_entity, queue_link, queue_relation, GraphOutbox, Origin};
use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

const ENTITY_SELECT: &str = "SELECT id, name, kind, aliases, created_at, updated_at FROM entities";

fn parse_entity_row(row: &Row) -> rusqlite::Result<Entity> {
    let aliases_json: String = row.get("aliases")?;
    Ok(Entity {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// `aliases` as the JSON array the column holds.
fn aliases_json(aliases: &[String]) -> String {
    serde_json::to_string(aliases).unwrap_or_else(|_| "[]".to_string())
}

/// What a sync merge needs of the local copy of an entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySyncView {
    pub aliases: Vec<String>,
    pub updated_at: String,
}

/// A whole `entity_relations` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationRow<'a> {
    pub id: &'a str,
    pub subject_entity_id: &'a str,
    pub relation: &'a str,
    pub object_entity_id: &'a str,
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub node_id: Option<&'a str>,
}

/// A relation as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRelation {
    pub id: String,
    pub subject_entity_id: String,
    pub relation: String,
    pub object_entity_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The knowledge-graph tables, over one connection, or on the engine's
/// memories core when the store's tables hold it (`db::engine::graph`).
pub struct Entities<'c> {
    conn: &'c Connection,
    /// Where writes queue their outbox entries on SQLite.
    outbox: GraphOutbox<'c>,
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> Entities<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            outbox: GraphOutbox::new(store),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    // --- entities --------------------------------------------------------

    /// The entity with id `id`, if there is one.
    pub fn get(&self, id: &str) -> Result<Option<Entity>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::get(&core.lock(), id);
        }
        Ok(self
            .conn
            .query_row(
                &format!("{ENTITY_SELECT} WHERE id = ?"),
                params![id],
                parse_entity_row,
            )
            .optional()?)
    }

    /// Whether an entity with id `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::exists(&core.lock(), id);
        }
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM entities WHERE id = ?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(found.is_some())
    }

    /// Every entity, oldest first (ties by id).
    pub fn all_oldest_first(&self) -> Result<Vec<Entity>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::all_oldest_first(&core.lock());
        }
        let mut stmt = self
            .conn
            .prepare(&format!("{ENTITY_SELECT} ORDER BY created_at, id"))?;
        let rows = stmt
            .query_map([], parse_entity_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Every entity, in storage order (on the engine, by id).
    pub fn all(&self) -> Result<Vec<Entity>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::all(&core.lock());
        }
        let mut stmt = self.conn.prepare(ENTITY_SELECT)?;
        let rows = stmt
            .query_map([], parse_entity_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Insert `entity`, made on this node, created and updated at its own
    /// stamps, recording `node_id` as where it was made. An existing id is an
    /// error.
    pub fn insert(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::insert(&mut core.lock(), entity, node_id);
        }
        self.conn.execute(
            "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, node_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                entity.id,
                entity.name,
                entity.kind,
                aliases_json(&entity.aliases),
                entity.created_at,
                entity.updated_at,
                node_id,
            ],
        )?;
        queue_entity(self.outbox, &entity.id, "insert")
    }

    /// Set `id`'s kind and aliases, stamping `updated_at`.
    pub fn set_kind_and_aliases(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        updated_at: &str,
    ) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::set_kind_and_aliases(
                &mut core.lock(),
                id,
                kind,
                aliases,
                updated_at,
            );
        }
        self.conn.execute(
            "UPDATE entities SET kind = ?, aliases = ?, updated_at = ? WHERE id = ?",
            params![kind, aliases_json(aliases), updated_at, id],
        )?;
        queue_entity(self.outbox, id, "update")
    }

    /// How many entities there are.
    pub fn count(&self) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::count(&core.lock());
        }
        let total: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entities", [], |row| row.get(0))?;
        Ok(total.max(0) as usize)
    }

    /// A page of entities with their mention counts, most-mentioned first,
    /// then by name, then id.
    pub fn page_by_mentions(&self, limit: usize, offset: usize) -> Result<Vec<EntityListItem>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::page_by_mentions(&core.lock(), limit, offset);
        }
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.name, e.kind, e.aliases, e.updated_at,
                    count(me.memory_id) AS mention_count
               FROM entities e
          LEFT JOIN memory_entities me ON me.entity_id = e.id
           GROUP BY e.id
           ORDER BY mention_count DESC, e.name ASC, e.id
              LIMIT ? OFFSET ?",
        )?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                let aliases_json: String = row.get("aliases")?;
                Ok(EntityListItem {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    kind: row.get("kind")?,
                    aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                    updated_at: row.get("updated_at")?,
                    mention_count: row.get("mention_count")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    // --- id renormalisation ----------------------------------------------

    /// Rename the entity `from` to `to`, where `to` is free. Links and
    /// relations are repointed separately. Queued for sync as an update of
    /// `to`.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::rename(&mut core.lock(), from, to);
        }
        self.conn
            .execute("UPDATE entities SET id = ? WHERE id = ?", params![to, from])?;
        queue_entity(self.outbox, to, "update")
    }

    /// Set `id`'s kind, aliases and `created_at`, without stamping
    /// `updated_at`: the merge of two rows that were always one entity.
    pub fn set_merged(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        created_at: &str,
    ) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::set_merged(&mut core.lock(), id, kind, aliases, created_at);
        }
        self.conn.execute(
            "UPDATE entities SET kind = ?, aliases = ?, created_at = ? WHERE id = ?",
            params![kind, aliases_json(aliases), created_at, id],
        )?;
        queue_entity(self.outbox, id, "update")
    }

    /// Delete the entity `id`. Its links and relations are not touched.
    pub fn delete(&self, id: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::delete(&mut core.lock(), id);
        }
        self.conn
            .execute("DELETE FROM entities WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Repoint every mention link and relation end from the entity `from` to
    /// `to`. A link `to` already has is dropped rather than duplicated.
    pub fn repoint(&self, from: &str, to: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::repoint(&mut core.lock(), from, to);
        }
        // `memory_entities` is keyed `(memory_id, entity_id)`, so repointing
        // can collide with a link `to` already has. Ignore those, then drop
        // whatever the ignore left behind.
        self.conn.execute(
            "UPDATE OR IGNORE memory_entities SET entity_id = ? WHERE entity_id = ?",
            params![to, from],
        )?;
        self.conn.execute(
            "DELETE FROM memory_entities WHERE entity_id = ?",
            params![from],
        )?;
        // Relations are keyed on their own id, so these cannot collide.
        self.conn.execute(
            "UPDATE entity_relations SET subject_entity_id = ? WHERE subject_entity_id = ?",
            params![to, from],
        )?;
        self.conn.execute(
            "UPDATE entity_relations SET object_entity_id = ? WHERE object_entity_id = ?",
            params![to, from],
        )?;
        Ok(())
    }

    // --- mention links ---------------------------------------------------

    /// Every mention link as `(memory_id, entity_id, created_at)`, oldest
    /// first.
    pub fn links_oldest_first(&self) -> Result<Vec<(String, String, String)>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::links_oldest_first(&core.lock());
        }
        let mut stmt = self.conn.prepare(
            "SELECT memory_id, entity_id, created_at FROM memory_entities
              ORDER BY created_at, memory_id, entity_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Record that `memory_id` mentions `entity_id`. Returns whether the
    /// link is new: links are immutable, so an existing one is left as is.
    pub fn link(
        &self,
        memory_id: &str,
        entity_id: &str,
        created_at: &str,
        origin: Origin,
    ) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::link(&mut core.lock(), memory_id, entity_id, created_at, origin);
        }
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id, created_at)
             VALUES (?, ?, ?)",
            params![memory_id, entity_id, created_at],
        )? > 0;
        if inserted && origin == Origin::Local {
            queue_link(self.outbox, memory_id, entity_id)?;
        }
        Ok(inserted)
    }

    /// Remove every mention link from `memory_id`: part of deleting a
    /// memory, since the table has no foreign key to cascade.
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::unlink_memory(&mut core.lock(), memory_id);
        }
        self.conn.execute(
            "DELETE FROM memory_entities WHERE memory_id = ?",
            params![memory_id],
        )?;
        Ok(())
    }

    /// Live memories linked to `entity_id`, newest first (ties by id,
    /// descending), at most `limit`,
    /// with the first 300 characters of their content. A link to a memory
    /// not stored here is skipped.
    pub fn linked_memories(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<EntityLinkedMemory>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::linked_memories(&core.lock(), entity_id, limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, substr(m.content, 1, 300) AS content_snippet, m.category, m.created_at
               FROM memory_entities me
               JOIN memories m ON m.id = me.memory_id
              WHERE me.entity_id = ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL
              ORDER BY m.created_at DESC, m.id DESC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![entity_id, limit as i64], |row| {
                Ok(EntityLinkedMemory {
                    id: row.get("id")?,
                    content_snippet: row.get("content_snippet")?,
                    category: row.get("category")?,
                    created_at: row.get("created_at")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many live memories are linked to `entity_id`.
    pub fn linked_memory_count(&self, entity_id: &str) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::linked_memory_count(&core.lock(), entity_id);
        }
        let count: i64 = self.conn.query_row(
            "SELECT count(*)
               FROM memory_entities me
               JOIN memories m ON m.id = me.memory_id
              WHERE me.entity_id = ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL",
            params![entity_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Live memories whose subject or object, lowercased, is `canonical`,
    /// newest first (ties by id, descending), at most `limit`.
    pub fn facts_naming(&self, canonical: &str, limit: usize) -> Result<Vec<EntityFact>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::facts_naming(&core.lock(), canonical, limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, content, subject, predicate, object, category, created_at
               FROM memories
              WHERE superseded_by IS NULL AND deleted_at IS NULL
                AND (lower(subject) = ? OR lower(object) = ?)
              ORDER BY created_at DESC, id DESC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![canonical, canonical, limit as i64], |row| {
                Ok(EntityFact {
                    id: row.get("id")?,
                    content: row.get("content")?,
                    subject: row.get("subject")?,
                    predicate: row.get("predicate")?,
                    object: row.get("object")?,
                    category: row.get("category")?,
                    created_at: row.get("created_at")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    // --- relations -------------------------------------------------------

    /// Every relation, oldest first (ties by id).
    pub fn relations_oldest_first(&self) -> Result<Vec<StoredRelation>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::relations_oldest_first(&core.lock());
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, subject_entity_id, relation, object_entity_id, created_at, updated_at
               FROM entity_relations ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StoredRelation {
                    id: r.get(0)?,
                    subject_entity_id: r.get(1)?,
                    relation: r.get(2)?,
                    object_entity_id: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted: relations are immutable.
    pub fn insert_relation_or_ignore(&self, row: &RelationRow<'_>, origin: Origin) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::insert_relation_or_ignore(&mut core.lock(), row, origin);
        }
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO entity_relations
                 (id, subject_entity_id, relation, object_entity_id, created_at, updated_at, node_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                row.id,
                row.subject_entity_id,
                row.relation,
                row.object_entity_id,
                row.created_at,
                row.updated_at,
                row.node_id,
            ],
        )? > 0;
        if inserted && origin == Origin::Local {
            queue_relation(self.outbox, row.id)?;
        }
        Ok(inserted)
    }

    /// Every relation with either end in `entity_ids`, and `relation` as its
    /// label when one is given, oldest first (ties by id). Each comes with its id, and a
    /// relation whose ends are not both stored here is skipped. `hop` is
    /// copied onto each edge.
    pub fn relations_touching(
        &self,
        entity_ids: &[String],
        relation: Option<&str>,
        hop: u32,
    ) -> Result<Vec<(String, RelationEdge)>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::relations_touching(&core.lock(), entity_ids, relation, hop);
        }
        let placeholders = vec!["?"; entity_ids.len()].join(",");
        let relation_clause = if relation.is_some() {
            " AND r.relation = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT r.id, r.subject_entity_id, r.relation, r.object_entity_id,
                    s.name AS subject_name, s.kind AS subject_kind,
                    o.name AS object_name, o.kind AS object_kind
               FROM entity_relations r
               JOIN entities s ON s.id = r.subject_entity_id
               JOIN entities o ON o.id = r.object_entity_id
              WHERE (r.subject_entity_id IN ({placeholders})
                     OR r.object_entity_id IN ({placeholders})){relation_clause}
              ORDER BY r.created_at, r.id"
        );

        // The ids are bound twice, once per side of the OR.
        let mut bindings: Vec<SqlValue> = entity_ids
            .iter()
            .chain(entity_ids.iter())
            .map(|id| SqlValue::Text(id.clone()))
            .collect();
        if let Some(label) = relation {
            bindings.push(SqlValue::Text(label.to_string()));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings), |row| {
                Ok((
                    row.get::<_, String>("id")?,
                    RelationEdge {
                        subject_entity_id: row.get("subject_entity_id")?,
                        subject_name: row.get("subject_name")?,
                        subject_kind: row.get("subject_kind")?,
                        relation: row.get("relation")?,
                        object_entity_id: row.get("object_entity_id")?,
                        object_name: row.get("object_name")?,
                        object_kind: row.get("object_kind")?,
                        hop,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    // --- sync ------------------------------------------------------------

    /// The local copy of `id` as a sync merge sees it, if there is one.
    pub fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::sync_view(&core.lock(), id);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT aliases, updated_at FROM entities WHERE id = ?",
                params![id],
                |row| {
                    let aliases_json: String = row.get(0)?;
                    Ok(EntitySyncView {
                        aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                        updated_at: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Write an entity a peer sent: insert it, or overwrite the local row's
    /// name, kind, aliases, `updated_at` and `node_id`. The local
    /// `created_at` is kept. Never queued for sync: it came from there.
    pub fn upsert_synced(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::upsert_synced(&mut core.lock(), entity, node_id);
        }
        self.conn.execute(
            "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, node_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 kind = excluded.kind,
                 aliases = excluded.aliases,
                 updated_at = excluded.updated_at,
                 node_id = excluded.node_id",
            params![
                entity.id,
                entity.name,
                entity.kind,
                aliases_json(&entity.aliases),
                entity.created_at,
                entity.updated_at,
                node_id,
            ],
        )?;
        Ok(())
    }

    /// Replace `id`'s aliases without stamping `updated_at`: a sync merge
    /// that lost last-write-wins still keeps the union. Never queued for
    /// sync.
    pub fn set_aliases(&self, id: &str, aliases: &[String]) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::graph::set_aliases(&mut core.lock(), id, aliases);
        }
        self.conn.execute(
            "UPDATE entities SET aliases = ? WHERE id = ?",
            params![aliases_json(aliases), id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn entity(id: &str, name: &str, created_at: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: name.to_string(),
            kind: None,
            aliases: vec!["alias".to_string()],
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
        }
    }

    /// Every graph write on `db` with sync on, and every read after, with the
    /// outbox it queued.
    fn exercise(db: &Database) -> Vec<String> {
        use crate::db::derived::Origin::{Local, Sync};
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::outbox::Outbox;
        use crate::db::sync_state::SyncState;
        let store = db.store();
        SyncState::new(&store)
            .set_flag("sync_enabled", "1")
            .unwrap();
        let memories = Memories::new(&store);
        for (id, at, subject) in [
            ("m1", T1, Some("RUST")),
            ("m2", T2, None),
            ("m3", T2, Some("rust")),
            ("m4", T1, None),
            ("m5", T2, None),
        ] {
            memories
                .insert(&NewMemory {
                    subject: subject.map(String::from),
                    ..NewMemory::new(id, format!("{id} {}", "ß".repeat(400)), at)
                })
                .unwrap();
        }
        memories.set_superseded_by("m4", "m1", None).unwrap();
        let entities = Entities::new(&store);
        let mut seen = Vec::new();
        entities.insert(&entity("rust", "Rust", T1), None).unwrap();
        entities
            .insert(&entity("tokio", "Tokio", T2), Some("n1"))
            .unwrap();
        entities.insert(&entity("old", "Old", T1), None).unwrap();
        seen.push(format!(
            "{}",
            entities.insert(&entity("rust", "Again", T2), None).is_err()
        ));
        entities
            .set_kind_and_aliases("tokio", Some("lib"), &["tk".into()], T2)
            .unwrap();
        entities
            .set_merged("old", Some("x"), &["o".into()], T2)
            .unwrap();
        for (m, e, origin) in [
            ("m1", "rust", Local),
            ("m2", "rust", Sync),
            ("m4", "rust", Local),
            ("m1", "old", Local),
            ("m3", "old", Local),
            ("gone", "old", Local),
        ] {
            seen.push(format!("{}", entities.link(m, e, T1, origin).unwrap()));
        }
        seen.push(format!(
            "{}",
            entities.link("m1", "rust", T2, Local).unwrap()
        ));
        for (id, s, o, origin) in [
            ("r1", "rust", "tokio", Local),
            ("r2", "tokio", "old", Sync),
            ("r3", "old", "nowhere", Local),
        ] {
            let row = RelationRow {
                id,
                subject_entity_id: s,
                relation: "uses",
                object_entity_id: o,
                created_at: T1,
                updated_at: T1,
                node_id: None,
            };
            seen.push(format!(
                "{}",
                entities.insert_relation_or_ignore(&row, origin).unwrap()
            ));
        }
        seen.push(format!("{:?}", entities.page_by_mentions(10, 0).unwrap()));
        entities.repoint("old", "rust").unwrap();
        entities.rename("tokio", "tokio2").unwrap();
        entities.delete("old").unwrap();
        entities
            .upsert_synced(&entity("rust", "Rust!", T2), Some("peer"))
            .unwrap();
        entities
            .upsert_synced(&entity("new", "New", T2), None)
            .unwrap();
        entities
            .set_aliases("new", &["n".into(), "nn".into()])
            .unwrap();
        Entities::new(&store).unlink_memory("m3").unwrap();

        seen.push(format!("{:?}", entities.get("rust").unwrap()));
        seen.push(format!("{:?}", entities.get("tokio").unwrap()));
        seen.push(format!("{}", entities.exists("tokio2").unwrap()));
        seen.push(format!("{}", entities.count().unwrap()));
        seen.push(format!("{:?}", entities.all_oldest_first().unwrap()));
        seen.push(format!("{:?}", entities.page_by_mentions(2, 1).unwrap()));
        seen.push(format!("{:?}", entities.links_oldest_first().unwrap()));
        seen.push(format!(
            "{:?}",
            entities.linked_memories("rust", 10).unwrap()
        ));
        seen.push(format!("{}", entities.linked_memory_count("rust").unwrap()));
        seen.push(format!("{:?}", entities.facts_naming("rust", 10).unwrap()));
        seen.push(format!("{:?}", entities.relations_oldest_first().unwrap()));
        seen.push(format!(
            "{:?}",
            entities
                .relations_touching(&["rust".into(), "tokio2".into()], None, 2)
                .unwrap()
        ));
        seen.push(format!(
            "{:?}",
            entities
                .relations_touching(&["rust".into()], Some("owns"), 1)
                .unwrap()
        ));
        seen.push(format!("{:?}", entities.sync_view("new").unwrap()));
        seen.push(format!("{:?}", memories.unannotated_page(10).unwrap()));
        let queued = |outbox: &Outbox<'_>| -> Vec<(String, serde_json::Value)> {
            outbox
                .unsent_to("hub", 0, 1000)
                .unwrap()
                .into_iter()
                .filter(|e| !e.key.starts_with('m') || e.payload_json.contains("record_type"))
                .map(|e| (e.key, serde_json::from_str(&e.payload_json).unwrap()))
                .collect()
        };
        let outbox = Outbox::new(&store);
        seen.push(format!("{:?}", queued(&outbox)));
        outbox.clear().unwrap();
        outbox.backfill_everything().unwrap();
        let mut backfilled: Vec<String> = queued(&outbox)
            .into_iter()
            .map(|(key, payload)| format!("{key} {payload}"))
            .collect();
        backfilled.sort();
        seen.push(format!("{backfilled:?}"));
        seen
    }

    #[test]
    fn the_engine_core_keeps_the_graph_as_sqlite_does() {
        let mut observed = Vec::new();
        crate::db::on_each_core_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite[0], "true", "a taken entity id is refused");
        for other in &observed[1..] {
            for (theirs, ours) in other.iter().zip(sqlite) {
                assert_eq!(theirs, ours);
            }
            assert_eq!(other.len(), sqlite.len());
        }
    }

    #[test]
    fn a_synced_overwrite_keeps_created_at() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let entities = Entities::new(&store);
        entities.insert(&entity("e1", "Local", T1), None).unwrap();
        entities
            .upsert_synced(&entity("e1", "Remote", T2), Some("peer"))
            .unwrap();

        let stored = entities.get("e1").unwrap().unwrap();
        assert_eq!(stored.name, "Remote");
        assert_eq!(stored.created_at, T1);
        assert_eq!(stored.updated_at, T2);
    }

    #[test]
    fn repoint_drops_a_link_the_target_already_has() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let entities = Entities::new(&store);
        assert!(entities
            .link("m1", "old", T1, crate::db::derived::Origin::Local)
            .unwrap());
        assert!(entities
            .link("m1", "new", T1, crate::db::derived::Origin::Local)
            .unwrap());
        assert!(entities
            .link("m2", "old", T1, crate::db::derived::Origin::Local)
            .unwrap());
        assert!(!entities
            .link("m2", "old", T2, crate::db::derived::Origin::Local)
            .unwrap());

        entities.repoint("old", "new").unwrap();

        let mut links: Vec<(String, String)> = entities
            .links_oldest_first()
            .unwrap()
            .into_iter()
            .map(|(memory_id, entity_id, _)| (memory_id, entity_id))
            .collect();
        links.sort();
        assert_eq!(
            links,
            vec![
                ("m1".to_string(), "new".to_string()),
                ("m2".to_string(), "new".to_string())
            ]
        );
    }

    #[test]
    fn relations_touching_nothing_is_empty() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        assert!(Entities::new(&store)
            .relations_touching(&[], None, 1)
            .unwrap()
            .is_empty());
    }
}
