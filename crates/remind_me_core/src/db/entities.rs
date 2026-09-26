//! Storage for the knowledge graph: `entities`, `entity_relations` and the
//! `memory_entities` mention links.
//!
//! ADR-0023 phase 1, step 2. Every statement [`crate::entity`] and
//! [`crate::sync::graph`] ran lives here. The rules stay with them: how a
//! name normalises into an id, how aliases merge, which kind wins, how a
//! traversal walks, and how sync resolves a conflict.

use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Result, Row};

const ENTITY_SELECT: &str = "SELECT id, name, kind, aliases, created_at, updated_at FROM entities";

fn parse_entity_row(row: &Row) -> Result<Entity> {
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

/// The knowledge-graph tables, over one connection.
pub struct Entities<'c> {
    conn: &'c Connection,
}

impl<'c> Entities<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    // --- entities --------------------------------------------------------

    /// The entity with id `id`, if there is one.
    pub fn get(&self, id: &str) -> Result<Option<Entity>> {
        self.conn
            .query_row(
                &format!("{ENTITY_SELECT} WHERE id = ?"),
                params![id],
                parse_entity_row,
            )
            .optional()
    }

    /// Whether an entity with id `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
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
        let mut stmt = self
            .conn
            .prepare(&format!("{ENTITY_SELECT} ORDER BY created_at, id"))?;
        let rows = stmt.query_map([], parse_entity_row)?.collect();
        rows
    }

    /// Every entity, in storage order.
    pub fn all(&self) -> Result<Vec<Entity>> {
        let mut stmt = self.conn.prepare(ENTITY_SELECT)?;
        let rows = stmt.query_map([], parse_entity_row)?.collect();
        rows
    }

    /// Insert `entity`, created and updated at its own stamps, recording
    /// `node_id` as its origin. An existing id is an error.
    pub fn insert(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
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
        Ok(())
    }

    /// Set `id`'s kind and aliases, stamping `updated_at`.
    pub fn set_kind_and_aliases(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        updated_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE entities SET kind = ?, aliases = ?, updated_at = ? WHERE id = ?",
            params![kind, aliases_json(aliases), updated_at, id],
        )?;
        Ok(())
    }

    /// How many entities there are.
    pub fn count(&self) -> Result<usize> {
        let total: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entities", [], |row| row.get(0))?;
        Ok(total.max(0) as usize)
    }

    /// A page of entities with their mention counts, most-mentioned first,
    /// then by name.
    pub fn page_by_mentions(&self, limit: usize, offset: usize) -> Result<Vec<EntityListItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.name, e.kind, e.aliases, e.updated_at,
                    count(me.memory_id) AS mention_count
               FROM entities e
          LEFT JOIN memory_entities me ON me.entity_id = e.id
           GROUP BY e.id
           ORDER BY mention_count DESC, e.name ASC
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
            .collect();
        rows
    }

    // --- id renormalisation ----------------------------------------------

    /// Rename the entity `from` to `to`, where `to` is free. Links and
    /// relations are repointed separately.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.conn
            .execute("UPDATE entities SET id = ? WHERE id = ?", params![to, from])?;
        Ok(())
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
        self.conn.execute(
            "UPDATE entities SET kind = ?, aliases = ?, created_at = ? WHERE id = ?",
            params![kind, aliases_json(aliases), created_at, id],
        )?;
        Ok(())
    }

    /// Delete the entity `id`. Its links and relations are not touched.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM entities WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Repoint every mention link and relation end from the entity `from` to
    /// `to`. A link `to` already has is dropped rather than duplicated.
    pub fn repoint(&self, from: &str, to: &str) -> Result<()> {
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

    /// Record that `memory_id` mentions `entity_id`. Returns whether the
    /// link is new: links are immutable, so an existing one is left as is.
    pub fn link(&self, memory_id: &str, entity_id: &str, created_at: &str) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id, created_at)
             VALUES (?, ?, ?)",
            params![memory_id, entity_id, created_at],
        )?;
        Ok(inserted > 0)
    }

    /// Live memories linked to `entity_id`, newest first, at most `limit`,
    /// with the first 300 characters of their content. A link to a memory
    /// not stored here is skipped.
    pub fn linked_memories(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<EntityLinkedMemory>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, substr(m.content, 1, 300) AS content_snippet, m.category, m.created_at
               FROM memory_entities me
               JOIN memories m ON m.id = me.memory_id
              WHERE me.entity_id = ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL
              ORDER BY m.created_at DESC
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
            .collect();
        rows
    }

    /// How many live memories are linked to `entity_id`.
    pub fn linked_memory_count(&self, entity_id: &str) -> Result<usize> {
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
    /// newest first, at most `limit`.
    pub fn facts_naming(&self, canonical: &str, limit: usize) -> Result<Vec<EntityFact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, subject, predicate, object, category, created_at
               FROM memories
              WHERE superseded_by IS NULL AND deleted_at IS NULL
                AND (lower(subject) = ? OR lower(object) = ?)
              ORDER BY created_at DESC
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
            .collect();
        rows
    }

    // --- relations -------------------------------------------------------

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted: relations are immutable.
    pub fn insert_relation_or_ignore(&self, row: &RelationRow<'_>) -> Result<bool> {
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
        )?;
        Ok(inserted > 0)
    }

    /// Every relation with either end in `entity_ids`, and `relation` as its
    /// label when one is given, oldest first. Each comes with its id, and a
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
              ORDER BY r.created_at"
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
            .collect();
        rows
    }

    // --- sync ------------------------------------------------------------

    /// The local copy of `id` as a sync merge sees it, if there is one.
    pub fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>> {
        self.conn
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
            .optional()
    }

    /// Write an entity a peer sent: insert it, or overwrite the local row's
    /// name, kind, aliases, `updated_at` and `node_id`. The local
    /// `created_at` is kept.
    pub fn upsert_synced(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
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
    /// that lost last-write-wins still keeps the union.
    pub fn set_aliases(&self, id: &str, aliases: &[String]) -> Result<()> {
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

    #[test]
    fn a_synced_overwrite_keeps_created_at() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let entities = Entities::new(&conn);
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
        let conn = db.conn();
        let entities = Entities::new(&conn);
        assert!(entities.link("m1", "old", T1).unwrap());
        assert!(entities.link("m1", "new", T1).unwrap());
        assert!(entities.link("m2", "old", T1).unwrap());
        assert!(!entities.link("m2", "old", T2).unwrap());

        entities.repoint("old", "new").unwrap();

        let mut links: Vec<(String, String)> = conn
            .prepare("SELECT memory_id, entity_id FROM memory_entities ORDER BY memory_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
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
        let conn = db.conn();
        assert!(Entities::new(&conn)
            .relations_touching(&[], None, 1)
            .unwrap()
            .is_empty());
    }
}
