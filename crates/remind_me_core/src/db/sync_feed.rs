//! Storage reads for the peer sync server: the four pull feeds (memories,
//! entities, mention links and relations), each keyset-paged, as the wire
//! records a pulling node applies, plus the graph table counts `/count`
//! reports.
//!
//! ADR-0023 phase 1, step 8. Every statement [`crate::sync::server`] ran
//! lives here. The server keeps the rules: parameter defaults, the page
//! limit, and the response shapes.

use rusqlite::{params, Connection, Result};
use serde_json::{json, Value};

const SYNC_RECORD_COLUMNS: &str =
    "id, content, category, tags, source, metadata, created_at, updated_at, \
     capture_id, node_id, client, accessed_at, access_count, decay_rate, vitality, base_weight, \
     status, memory_type, source_capture_id, subject, predicate, object, superseded_by, \
     deleted_at, sensitive, remind_at";

fn memory_record(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let tags_json: String = row.get("tags")?;
    let metadata_json: String = row.get("metadata")?;
    Ok(json!({
        "id": row.get::<_, String>("id")?,
        "content": row.get::<_, String>("content")?,
        "category": row.get::<_, String>("category")?,
        "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or_else(|_| json!([])),
        "source": row.get::<_, String>("source")?,
        "metadata": serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({})),
        "created_at": row.get::<_, String>("created_at")?,
        "updated_at": row.get::<_, String>("updated_at")?,
        "capture_id": row.get::<_, Option<String>>("capture_id")?,
        "node_id": row.get::<_, Option<String>>("node_id")?,
        "client": row.get::<_, String>("client")?,
        "accessed_at": row.get::<_, Option<String>>("accessed_at")?,
        "access_count": row.get::<_, i64>("access_count")?,
        "decay_rate": row.get::<_, f64>("decay_rate")?,
        "vitality": row.get::<_, f64>("vitality")?,
        "base_weight": row.get::<_, f64>("base_weight")?,
        "status": row.get::<_, String>("status")?,
        "memory_type": row.get::<_, String>("memory_type")?,
        "source_capture_id": row.get::<_, Option<String>>("source_capture_id")?,
        "subject": row.get::<_, Option<String>>("subject")?,
        "predicate": row.get::<_, Option<String>>("predicate")?,
        "object": row.get::<_, Option<String>>("object")?,
        "superseded_by": row.get::<_, Option<String>>("superseded_by")?,
        // Three columns this function never read (#265). `deleted_at` is the
        // most serious of the three found while fixing `sensitive`/
        // `remind_at`: without it, a tombstone never propagates over direct
        // peer sync at all, so a memory deleted on one node and pulled by a
        // peer stays live there forever. `sync/record.rs`'s `SyncRecord`
        // already expects all three on the wire; this function just never
        // supplied them.
        "deleted_at": row.get::<_, Option<String>>("deleted_at")?,
        "sensitive": row.get::<_, bool>("sensitive")?,
        "remind_at": row.get::<_, Option<String>>("remind_at")?,
    }))
}

fn entity_record(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let aliases_json: String = row.get("aliases")?;
    Ok(json!({
        "record_type": "entity",
        "id": row.get::<_, String>("id")?,
        "name": row.get::<_, String>("name")?,
        "kind": row.get::<_, Option<String>>("kind")?,
        "aliases": serde_json::from_str::<Value>(&aliases_json).unwrap_or_else(|_| json!([])),
        "created_at": row.get::<_, String>("created_at")?,
        "updated_at": row.get::<_, String>("updated_at")?,
        "node_id": row.get::<_, Option<String>>("node_id")?,
    }))
}

/// The pull feeds, over one connection.
pub struct SyncFeed<'c> {
    conn: &'c Connection,
}

impl<'c> SyncFeed<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Up to `limit` memories after the cursor `(since, since_id)` on
    /// `(updated_at, id)`, oldest first, skipping those `exclude_node` wrote
    /// when given.
    pub fn memories_after(
        &self,
        since: &str,
        since_id: &str,
        exclude_node: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let sql = format!(
            "SELECT {SYNC_RECORD_COLUMNS} FROM memories
              WHERE (updated_at > ?1 OR (updated_at = ?1 AND id > ?2))
                {}
              ORDER BY updated_at ASC, id ASC
              LIMIT ?3",
            exclude_clause(exclude_node)
        );
        self.page(&sql, since, since_id, exclude_node, limit, memory_record)
    }

    /// Up to `limit` entities after the cursor `(since, since_id)` on
    /// `(updated_at, id)`, oldest first, skipping those `exclude_node` wrote
    /// when given.
    pub fn entities_after(
        &self,
        since: &str,
        since_id: &str,
        exclude_node: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let sql = format!(
            "SELECT id, name, kind, aliases, created_at, updated_at, node_id FROM entities
              WHERE (updated_at > ?1 OR (updated_at = ?1 AND id > ?2))
                {}
              ORDER BY updated_at ASC, id ASC
              LIMIT ?3",
            exclude_clause(exclude_node)
        );
        self.page(&sql, since, since_id, exclude_node, limit, entity_record)
    }

    /// Up to `limit` mention links after the cursor `(since, since_id)` on
    /// `(created_at, memory_id|entity_id)`, oldest first. Links have no
    /// `node_id`, so there is nothing to exclude.
    pub fn links_after(&self, since: &str, since_id: &str, limit: usize) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT memory_id, entity_id, created_at FROM memory_entities
              WHERE (created_at > ?1 OR (created_at = ?1 AND (memory_id || '|' || entity_id) > ?2))
              ORDER BY created_at ASC, (memory_id || '|' || entity_id) ASC
              LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![since, since_id, limit as i64], |row| {
                let memory_id: String = row.get("memory_id")?;
                let entity_id: String = row.get("entity_id")?;
                Ok(json!({
                    "record_type": "memory_entity",
                    "id": format!("{memory_id}|{entity_id}"),
                    "memory_id": memory_id,
                    "entity_id": entity_id,
                    "created_at": row.get::<_, String>("created_at")?,
                }))
            })?
            .collect();
        rows
    }

    /// Up to `limit` relations after the cursor `(since, since_id)` on
    /// `(created_at, id)`, oldest first.
    pub fn relations_after(&self, since: &str, since_id: &str, limit: usize) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject_entity_id, relation, object_entity_id, created_at, updated_at, node_id
               FROM entity_relations
              WHERE (created_at > ?1 OR (created_at = ?1 AND id > ?2))
              ORDER BY created_at ASC, id ASC
              LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![since, since_id, limit as i64], |row| {
                Ok(json!({
                    "record_type": "entity_relation",
                    "id": row.get::<_, String>("id")?,
                    "subject_entity_id": row.get::<_, String>("subject_entity_id")?,
                    "relation": row.get::<_, String>("relation")?,
                    "object_entity_id": row.get::<_, String>("object_entity_id")?,
                    "created_at": row.get::<_, String>("created_at")?,
                    "updated_at": row.get::<_, String>("updated_at")?,
                    "node_id": row.get::<_, Option<String>>("node_id")?,
                }))
            })?
            .collect();
        rows
    }

    /// How many entities are stored.
    pub fn entity_count(&self) -> Result<i64> {
        self.count("entities")
    }

    /// How many mention links are stored.
    pub fn link_count(&self) -> Result<i64> {
        self.count("memory_entities")
    }

    /// How many relations are stored.
    pub fn relation_count(&self) -> Result<i64> {
        self.count("entity_relations")
    }

    fn count(&self, table: &str) -> Result<i64> {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
    }

    fn page(
        &self,
        sql: &str,
        since: &str,
        since_id: &str,
        exclude_node: Option<&str>,
        limit: usize,
        record: fn(&rusqlite::Row) -> Result<Value>,
    ) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = match exclude_node {
            Some(node) => stmt
                .query_map(params![since, since_id, limit as i64, node], record)?
                .collect(),
            None => stmt
                .query_map(params![since, since_id, limit as i64], record)?
                .collect(),
        };
        rows
    }
}

/// The condition that skips rows written by `exclude_node`, bound as `?4`.
fn exclude_clause(exclude_node: Option<&str>) -> &'static str {
    if exclude_node.is_some() {
        "AND (node_id IS NULL OR node_id != ?4)"
    } else {
        ""
    }
}
