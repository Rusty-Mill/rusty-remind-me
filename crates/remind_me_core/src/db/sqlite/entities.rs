//! The knowledge graph on SQLite: the [`EntityStore`] adapter over
//! `entities`, `entity_relations` and the `memory_entities` links (whose
//! DDL is with the memories, which read it). A local write queues its
//! outbox entry in the same savepoint, with the payloads the engine
//! queues.

use super::memories::{queue, rows_where, sync_enabled, text};
use super::stats::limit;
use super::{exec, SqliteTables};
use crate::db::derived::Origin;
use crate::db::engine::memories::MemoryRow;
use crate::db::entities::{EntityStore, EntitySyncView, RelationRow, StoredRelation};
use crate::db::{Result, StoreError};
use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};
use crate::models::Memory;
use rusqlite::{params, Connection, Row};
use serde_json::json;
use std::collections::HashSet;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT, aliases TEXT NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL, node_id TEXT);
CREATE TABLE IF NOT EXISTS entity_relations (
    id TEXT PRIMARY KEY, subject_entity_id TEXT NOT NULL, relation TEXT NOT NULL,
    object_entity_id TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    node_id TEXT);
CREATE INDEX IF NOT EXISTS entity_relations_subject ON entity_relations (subject_entity_id);
CREATE INDEX IF NOT EXISTS entity_relations_object ON entity_relations (object_entity_id);
";

const ENTITY: &str = "id, name, kind, aliases, created_at, updated_at, node_id";

/// An entity row: the entity, and the node that wrote it.
fn entity(r: &Row<'_>) -> rusqlite::Result<(Entity, Option<String>)> {
    Ok((
        Entity {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            aliases: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        },
        r.get(6)?,
    ))
}

fn aliases_json(aliases: &[String]) -> String {
    serde_json::to_string(aliases).unwrap_or_else(|_| "[]".to_string())
}

fn entity_payload(e: &Entity, node_id: Option<&str>) -> String {
    json!({
        "record_type": "entity", "id": e.id, "name": e.name, "kind": e.kind,
        "aliases": aliases_json(&e.aliases), "created_at": e.created_at,
        "updated_at": e.updated_at, "node_id": node_id,
    })
    .to_string()
}

fn link_payload(memory_id: &str, entity_id: &str, created_at: &str) -> String {
    json!({
        "record_type": "memory_entity", "id": format!("{memory_id}|{entity_id}"),
        "memory_id": memory_id, "entity_id": entity_id, "created_at": created_at,
    })
    .to_string()
}

fn relation_payload(r: &RelationRow<'_>) -> String {
    json!({
        "record_type": "entity_relation", "id": r.id, "subject_entity_id": r.subject_entity_id,
        "relation": r.relation, "object_entity_id": r.object_entity_id,
        "created_at": r.created_at, "updated_at": r.updated_at, "node_id": r.node_id,
    })
    .to_string()
}

/// Queue `payload` under `key` if the outbox is on.
fn queue_if_on(c: &Connection, key: &str, operation: &str, payload: &str) -> Result<()> {
    if sync_enabled(c)? {
        queue(c, key, operation, payload)?;
    }
    Ok(())
}

fn put_entity(c: &Connection, e: &Entity, node_id: Option<&str>) -> Result<()> {
    exec(
        c,
        "INSERT OR REPLACE INTO entities VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            e.id,
            e.name,
            e.kind,
            aliases_json(&e.aliases),
            e.created_at,
            e.updated_at,
            node_id
        ],
    )
    .map(drop)
}

/// Every entity and link as backfill inserts `(key, payload)`, oldest first.
pub(super) fn backfill_entries(t: &SqliteTables) -> Result<Vec<(String, String)>> {
    let entities = t.all(
        &format!("SELECT {ENTITY} FROM entities ORDER BY created_at, id"),
        [],
        entity,
    )?;
    let links = t.all(
        "SELECT memory_id, entity_id, created_at FROM memory_entities \
         ORDER BY created_at, memory_id, entity_id",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    Ok(entities
        .iter()
        .map(|(e, node)| (e.id.clone(), entity_payload(e, node.as_deref())))
        .chain(
            links
                .iter()
                .map(|(m, e, at)| (m.clone(), link_payload(m, e, at))),
        )
        .collect())
}

impl SqliteTables {
    fn stored_entity(&self, id: &str) -> Result<Option<(Entity, Option<String>)>> {
        self.one(
            &format!("SELECT {ENTITY} FROM entities WHERE id = ?1"),
            [id],
            entity,
        )
    }

    /// Change entity `id` if it exists, queueing an update when `queued`.
    fn update_entity(
        &mut self,
        id: &str,
        queued: bool,
        change: impl FnOnce(&mut Entity),
    ) -> Result<()> {
        let Some((mut e, node)) = self.stored_entity(id)? else {
            return Ok(());
        };
        change(&mut e);
        self.atomic(|c| {
            put_entity(c, &e, node.as_deref())?;
            match queued {
                true => queue_if_on(c, id, "update", &entity_payload(&e, node.as_deref())),
                false => Ok(()),
            }
        })
    }

    /// Live, unsuperseded memories linked to `entity_id`, newest first.
    fn linked_rows(&self, entity_id: &str) -> Result<Vec<MemoryRow>> {
        rows_where(
            &self.conn,
            "deleted_at IS NULL AND superseded_by IS NULL AND id IN \
             (SELECT memory_id FROM memory_entities WHERE entity_id = ?1)",
            "created_at DESC, id DESC",
            &[text(entity_id)],
        )
    }
}

impl EntityStore for SqliteTables {
    fn get(&self, id: &str) -> Result<Option<Entity>> {
        Ok(self.stored_entity(id)?.map(|(e, _)| e))
    }

    fn exists(&self, id: &str) -> Result<bool> {
        Ok(self.stored_entity(id)?.is_some())
    }

    fn all_oldest_first(&self) -> Result<Vec<Entity>> {
        let sql = format!("SELECT {ENTITY} FROM entities ORDER BY created_at, id");
        Ok(self
            .all(&sql, [], entity)?
            .into_iter()
            .map(|(e, _)| e)
            .collect())
    }

    fn all(&self) -> Result<Vec<Entity>> {
        let sql = format!("SELECT {ENTITY} FROM entities ORDER BY id");
        Ok(SqliteTables::all(self, &sql, [], entity)?
            .into_iter()
            .map(|(e, _)| e)
            .collect())
    }

    fn insert(&mut self, new: &Entity, node_id: Option<&str>) -> Result<()> {
        if self.stored_entity(&new.id)?.is_some() {
            return Err(StoreError::Engine(format!(
                "sqlite: entity {:?} already exists",
                new.id
            )));
        }
        self.atomic(|c| {
            put_entity(c, new, node_id)?;
            queue_if_on(c, &new.id, "insert", &entity_payload(new, node_id))
        })
    }

    fn set_kind_and_aliases(
        &mut self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        at: &str,
    ) -> Result<()> {
        self.update_entity(id, true, |e| {
            e.kind = kind.map(str::to_string);
            e.aliases = aliases.to_vec();
            e.updated_at = at.to_string();
        })
    }

    fn count(&self) -> Result<usize> {
        let n: i64 = self
            .one("SELECT count(*) FROM entities", [], |r| r.get(0))?
            .unwrap_or(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn page_by_mentions(&self, n: usize, offset: usize) -> Result<Vec<EntityListItem>> {
        SqliteTables::all(
            self,
            "SELECT e.id, e.name, e.kind, e.aliases, e.updated_at, \
             (SELECT count(*) FROM memory_entities l WHERE l.entity_id = e.id) AS mentions \
             FROM entities e ORDER BY mentions DESC, e.name, e.id LIMIT ?1 OFFSET ?2",
            params![limit(n), limit(offset)],
            |r| {
                Ok(EntityListItem {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    aliases: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    updated_at: r.get(4)?,
                    mention_count: r.get(5)?,
                })
            },
        )
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let Some((mut e, node)) = self.stored_entity(from)? else {
            return Ok(());
        };
        if self.stored_entity(to)?.is_some() {
            return Err(StoreError::Engine(format!(
                "sqlite: entity {to:?} already exists"
            )));
        }
        e.id = to.to_string();
        self.atomic(|c| {
            exec(c, "DELETE FROM entities WHERE id = ?1", [from])?;
            put_entity(c, &e, node.as_deref())?;
            queue_if_on(c, to, "update", &entity_payload(&e, node.as_deref()))
        })
    }

    fn set_merged(
        &mut self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        created_at: &str,
    ) -> Result<()> {
        self.update_entity(id, true, |e| {
            e.kind = kind.map(str::to_string);
            e.aliases = aliases.to_vec();
            e.created_at = created_at.to_string();
        })
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        self.exec("DELETE FROM entities WHERE id = ?1", [id])
            .map(drop)
    }

    fn repoint(&mut self, from: &str, to: &str) -> Result<()> {
        self.atomic(|c| {
            exec(
                c,
                "UPDATE OR IGNORE memory_entities SET entity_id = ?2 WHERE entity_id = ?1",
                [from, to],
            )?;
            exec(
                c,
                "DELETE FROM memory_entities WHERE entity_id = ?1",
                [from],
            )?;
            exec(
                c,
                "UPDATE entity_relations SET subject_entity_id = ?2 WHERE subject_entity_id = ?1",
                [from, to],
            )?;
            exec(
                c,
                "UPDATE entity_relations SET object_entity_id = ?2 WHERE object_entity_id = ?1",
                [from, to],
            )
            .map(drop)
        })
    }

    fn links_oldest_first(&self) -> Result<Vec<(String, String, String)>> {
        SqliteTables::all(
            self,
            "SELECT memory_id, entity_id, created_at FROM memory_entities \
             ORDER BY created_at, memory_id, entity_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }

    fn link(
        &mut self,
        memory_id: &str,
        entity_id: &str,
        created_at: &str,
        origin: Origin,
    ) -> Result<bool> {
        self.atomic(|c| {
            let added = exec(
                c,
                "INSERT OR IGNORE INTO memory_entities VALUES (?1, ?2, ?3)",
                [memory_id, entity_id, created_at],
            )? > 0;
            if added && origin == Origin::Local {
                queue_if_on(
                    c,
                    memory_id,
                    "insert",
                    &link_payload(memory_id, entity_id, created_at),
                )?;
            }
            Ok(added)
        })
    }

    fn unlink(&mut self, memory_id: &str, entity_id: &str) -> Result<bool> {
        Ok(self.exec(
            "DELETE FROM memory_entities WHERE memory_id = ?1 AND entity_id = ?2",
            [memory_id, entity_id],
        )? > 0)
    }

    fn unlink_memory(&mut self, memory_id: &str) -> Result<()> {
        self.exec(
            "DELETE FROM memory_entities WHERE memory_id = ?1",
            [memory_id],
        )
        .map(drop)
    }

    fn linked_memories(&self, entity_id: &str, n: usize) -> Result<Vec<EntityLinkedMemory>> {
        Ok(self
            .linked_rows(entity_id)?
            .into_iter()
            .take(n)
            .map(|row| EntityLinkedMemory {
                content_snippet: row.content.chars().take(300).collect(),
                id: row.id,
                category: row.category,
                created_at: row.created_at,
            })
            .collect())
    }

    fn linked_memories_full(&self, entity_id: &str) -> Result<Vec<Memory>> {
        Ok(self
            .linked_rows(entity_id)?
            .iter()
            .map(MemoryRow::to_memory)
            .collect())
    }

    fn linked_memory_count(&self, entity_id: &str) -> Result<usize> {
        Ok(self.linked_rows(entity_id)?.len())
    }

    fn entities_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        let mut found = std::collections::BTreeSet::new();
        for id in memory_ids {
            // The primary key leads with `memory_id`, so each is a seek.
            found.extend(self.all(
                "SELECT entity_id FROM memory_entities WHERE memory_id = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )?);
        }
        Ok(found.into_iter().collect())
    }

    fn shared_memory_count(&self, a: &str, b: &str) -> Result<usize> {
        let of_a: HashSet<String> = self.linked_rows(a)?.into_iter().map(|r| r.id).collect();
        Ok(self
            .linked_rows(b)?
            .iter()
            .filter(|r| of_a.contains(&r.id))
            .count())
    }

    fn facts_naming(&self, canonical: &str, n: usize) -> Result<Vec<EntityFact>> {
        // `lower()` folds ASCII only, as the engine compares.
        let rows = rows_where(
            &self.conn,
            "deleted_at IS NULL AND superseded_by IS NULL AND (lower(subject) = ?1 OR lower(object) = ?1)",
            &format!("created_at DESC, id DESC LIMIT {}", limit(n)),
            &[text(canonical)],
        )?;
        Ok(rows
            .into_iter()
            .map(|row| EntityFact {
                id: row.id,
                content: row.content,
                subject: row.subject,
                predicate: row.predicate,
                object: row.object,
                category: row.category,
                created_at: row.created_at,
            })
            .collect())
    }

    fn relations_oldest_first(&self) -> Result<Vec<StoredRelation>> {
        SqliteTables::all(
            self,
            "SELECT id, subject_entity_id, relation, object_entity_id, created_at, updated_at \
             FROM entity_relations ORDER BY created_at, id",
            [],
            |r| {
                Ok(StoredRelation {
                    id: r.get(0)?,
                    subject_entity_id: r.get(1)?,
                    relation: r.get(2)?,
                    object_entity_id: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            },
        )
    }

    fn insert_relation_or_ignore(&mut self, row: &RelationRow<'_>, origin: Origin) -> Result<bool> {
        self.atomic(|c| {
            let added = exec(
                c,
                "INSERT OR IGNORE INTO entity_relations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.id,
                    row.subject_entity_id,
                    row.relation,
                    row.object_entity_id,
                    row.created_at,
                    row.updated_at,
                    row.node_id
                ],
            )? > 0;
            if added && origin == Origin::Local {
                queue_if_on(c, row.id, "insert", &relation_payload(row))?;
            }
            Ok(added)
        })
    }

    fn relations_touching(
        &self,
        entity_ids: &[String],
        relation: Option<&str>,
        hop: u32,
    ) -> Result<Vec<(String, RelationEdge)>> {
        SqliteTables::all(
            self,
            "SELECT r.id, r.subject_entity_id, s.name, s.kind, r.relation, r.object_entity_id, o.name, o.kind \
             FROM entity_relations r JOIN entities s ON s.id = r.subject_entity_id \
             JOIN entities o ON o.id = r.object_entity_id \
             WHERE (r.subject_entity_id IN (SELECT value FROM json_each(?1)) \
                 OR r.object_entity_id IN (SELECT value FROM json_each(?1))) \
             AND (?2 IS NULL OR r.relation = ?2) ORDER BY r.created_at, r.id",
            params![super::imports::json_list(entity_ids), relation],
            |r| {
                Ok((
                    r.get(0)?,
                    RelationEdge {
                        subject_entity_id: r.get(1)?,
                        subject_name: r.get(2)?,
                        subject_kind: r.get(3)?,
                        relation: r.get(4)?,
                        object_entity_id: r.get(5)?,
                        object_name: r.get(6)?,
                        object_kind: r.get(7)?,
                        hop,
                    },
                ))
            },
        )
    }

    fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>> {
        Ok(self.stored_entity(id)?.map(|(e, _)| EntitySyncView {
            aliases: e.aliases,
            updated_at: e.updated_at,
        }))
    }

    fn upsert_synced(&mut self, new: &Entity, node_id: Option<&str>) -> Result<()> {
        let mut e = new.clone();
        if let Some((local, _)) = self.stored_entity(&new.id)? {
            e.created_at = local.created_at;
        }
        self.atomic(|c| put_entity(c, &e, node_id))
    }

    fn set_aliases(&mut self, id: &str, aliases: &[String]) -> Result<()> {
        self.update_entity(id, false, |e| e.aliases = aliases.to_vec())
    }

    fn linked_ids(&self, entity_id: &str) -> Result<HashSet<String>> {
        SqliteTables::linked_ids(self, entity_id)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::entities::entity_conformance!(SqliteTables::open_in_memory().unwrap());
}
