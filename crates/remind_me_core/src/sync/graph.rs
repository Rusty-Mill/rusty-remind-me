//! Sync for the knowledge-graph tables: `entities`, `entity_relations`, and
//! `memory_entities` mention links — the second slice of the sync epic,
//! continuing the `memories`-only slice this module's siblings implement.
//!
//! Verified against the reference's `sync.py`/`peer_server.py`/`db.py`
//! directly (not assumed) before writing any of this:
//!
//! - Entities get their own LWW conflict resolution (`name`/`kind`/`node_id`
//!   compared on `updated_at`, strict `>`), but `aliases` always union-merge
//!   regardless of the winner — the exact same "merge exception on top of
//!   LWW" shape `memories` uses for `tags`/`metadata`. This is a distinct,
//!   sync-specific function from the interactive `upsert_entity` (which has
//!   its own, different "existing kind wins" merge rule for direct tool
//!   calls) — the reference keeps these separate, and so does this port.
//! - `entity_relations` and `memory_entities` links are immutable —
//!   insert-or-ignore, no conflict resolution, no `updated_at` at all for
//!   links. A link or relation may reference a memory/entity that hasn't
//!   arrived yet: there is no foreign key, deliberately, so the row simply
//!   waits — nothing here retries it, the missing row just stops being
//!   missing whenever it eventually arrives.
//! - The generated schema (`schema_triggers.sql`, regenerated in `#76`) ships
//!   `entities_outbox_ai`/`_au`, `entity_relations_outbox_ai`, and
//!   `memory_entities_outbox_ai` itself, gated on `sync_flags.sync_enabled`
//!   exactly like `memories_outbox_ai`/`_au` — an earlier pass of this port
//!   installed hand-rolled, ungated equivalents because the schema dump this
//!   crate started from predated them; regenerating the dump found the real
//!   ones, so the hand-rolled copies were removed rather than kept as a
//!   second, diverging source of truth.

use super::record::canon_ts;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug)]
pub struct GraphApplyError(pub String);

impl std::fmt::Display for GraphApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for GraphApplyError {}
impl From<rusqlite::Error> for GraphApplyError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

fn before_outbox_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM sync_outbox", [], |r| {
        r.get(0)
    })
}

/// Echo-suppress whatever outbox row(s) this write itself just created for
/// `key` (the same `sync_outbox.memory_id` value the corresponding trigger
/// stamps) — identical technique to `record::upsert_record`'s, scoped by an
/// outbox-id high-water-mark snapshotted before the write.
fn echo_suppress(conn: &Connection, key: &str, before_id: i64) -> rusqlite::Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE sync_outbox SET sent_at = ? WHERE id > ? AND memory_id = ? AND sent_at = ''",
        params![now, before_id, key],
    )?;
    Ok(())
}

fn merge_aliases(local: &[String], incoming: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(local.len() + incoming.len());
    for alias in local.iter().chain(incoming.iter()) {
        if seen.insert(alias.clone()) {
            merged.push(alias.clone());
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySyncRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub node_id: Option<String>,
}

struct LocalEntity {
    aliases: Vec<String>,
    updated_at: String,
}

fn fetch_local_entity(conn: &Connection, id: &str) -> rusqlite::Result<Option<LocalEntity>> {
    conn.query_row(
        "SELECT aliases, updated_at FROM entities WHERE id = ?",
        params![id],
        |row| {
            let aliases_json: String = row.get(0)?;
            Ok(LocalEntity {
                aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
                updated_at: row.get(1)?,
            })
        },
    )
    .optional()
}

/// Apply one incoming `entity` record: LWW on `updated_at` governs
/// `name`/`kind`/`node_id` (a tie loses, same as `memories`), but `aliases`
/// always union-merges regardless of the winner, and a merge-only change
/// (the LWW loser's case) does not bump `updated_at`.
pub fn upsert_entity_record(
    conn: &Connection,
    record: &EntitySyncRecord,
) -> Result<(), GraphApplyError> {
    if record.id.trim().is_empty()
        || record.name.trim().is_empty()
        || record.updated_at.trim().is_empty()
    {
        return Err(GraphApplyError(
            "entity record is missing a required field (id/name/updated_at)".to_string(),
        ));
    }

    let updated_at = canon_ts(&record.updated_at);
    let created_at = canon_ts(&record.created_at);
    let local = fetch_local_entity(conn, &record.id)?;
    let incoming_wins = match &local {
        None => true,
        Some(l) => updated_at > l.updated_at,
    };
    let local_aliases: &[String] = local.as_ref().map(|l| l.aliases.as_slice()).unwrap_or(&[]);
    let merged_aliases = merge_aliases(local_aliases, &record.aliases);

    let before = before_outbox_id(conn)?;

    if incoming_wins {
        conn.execute(
            "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, node_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 kind = excluded.kind,
                 aliases = excluded.aliases,
                 updated_at = excluded.updated_at,
                 node_id = excluded.node_id",
            params![
                record.id,
                record.name,
                record.kind,
                serde_json::to_string(&merged_aliases).unwrap_or_else(|_| "[]".to_string()),
                created_at,
                updated_at,
                record.node_id,
            ],
        )?;
    } else {
        let local = local.expect("incoming_wins is false only when a local entity was found");
        if merged_aliases != local.aliases {
            conn.execute(
                "UPDATE entities SET aliases = ? WHERE id = ?",
                params![
                    serde_json::to_string(&merged_aliases).unwrap_or_else(|_| "[]".to_string()),
                    record.id
                ],
            )?;
        }
    }

    echo_suppress(conn, &record.id, before)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// entity_relations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRelationSyncRecord {
    pub id: String,
    pub subject_entity_id: String,
    pub relation: String,
    pub object_entity_id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Immutable, insert-or-ignore — the relation's id is already deterministic
/// (`entity_relation_id`), so a duplicate arriving twice (any order, from
/// any node) converges without any conflict resolution needed.
pub fn upsert_entity_relation_record(
    conn: &Connection,
    record: &EntityRelationSyncRecord,
) -> Result<(), GraphApplyError> {
    if record.id.trim().is_empty()
        || record.subject_entity_id.trim().is_empty()
        || record.relation.trim().is_empty()
        || record.object_entity_id.trim().is_empty()
    {
        return Err(GraphApplyError(
            "entity_relation record is missing a required field".to_string(),
        ));
    }

    let before = before_outbox_id(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO entity_relations
             (id, subject_entity_id, relation, object_entity_id, created_at, updated_at, node_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            record.id,
            record.subject_entity_id,
            record.relation,
            record.object_entity_id,
            canon_ts(&record.created_at),
            canon_ts(&record.updated_at),
            record.node_id,
        ],
    )?;
    echo_suppress(conn, &record.id, before)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// memory_entities (mention links)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSyncRecord {
    pub memory_id: String,
    pub entity_id: String,
    pub created_at: String,
}

/// Immutable, insert-or-ignore, no foreign key: a link may name a memory or
/// entity that has not arrived on this node yet (sync delivers rows out of
/// order). The row is inserted unconditionally and simply becomes visible
/// to every read path's `JOIN` the moment its referent shows up — nothing
/// here retries or reconciles it later.
pub fn upsert_link_record(
    conn: &Connection,
    record: &LinkSyncRecord,
) -> Result<String, GraphApplyError> {
    if record.memory_id.trim().is_empty() || record.entity_id.trim().is_empty() {
        return Err(GraphApplyError(
            "memory_entity record is missing memory_id/entity_id".to_string(),
        ));
    }

    let before = before_outbox_id(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id, created_at) VALUES (?, ?, ?)",
        params![
            record.memory_id,
            record.entity_id,
            canon_ts(&record.created_at)
        ],
    )?;
    // Echo-suppression is keyed on `memory_id` alone, matching the
    // `memory_entities_outbox_ai` trigger's own `sync_outbox.memory_id`
    // value -- not the synthetic `memory_id|entity_id` wire id.
    echo_suppress(conn, &record.memory_id, before)?;
    Ok(format!("{}|{}", record.memory_id, record.entity_id))
}

// ---------------------------------------------------------------------------
// Push-receiving dispatch
// ---------------------------------------------------------------------------

/// Apply one record from a `/sync/push` batch, dispatching on its
/// `record_type` field — absent means `"memory"`, matching the reference's
/// own wire convention exactly (memory payloads carry no discriminator at
/// all, for backward compatibility with pre-graph-sync peers). Returns the
/// wire id to report back in `processed_ids`.
pub fn apply_incoming_record(conn: &Connection, raw: &Value) -> Result<String, GraphApplyError> {
    let record_type = raw
        .get("record_type")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    match record_type {
        "memory" => {
            let record: super::SyncRecord =
                serde_json::from_value(raw.clone()).map_err(|e| GraphApplyError(e.to_string()))?;
            let id = record.id.clone();
            super::upsert_record(conn, &record).map_err(|e| GraphApplyError(e.to_string()))?;
            Ok(id)
        }
        "entity" => {
            let record: EntitySyncRecord =
                serde_json::from_value(raw.clone()).map_err(|e| GraphApplyError(e.to_string()))?;
            let id = record.id.clone();
            upsert_entity_record(conn, &record)?;
            Ok(id)
        }
        "entity_relation" => {
            let record: EntityRelationSyncRecord =
                serde_json::from_value(raw.clone()).map_err(|e| GraphApplyError(e.to_string()))?;
            let id = record.id.clone();
            upsert_entity_relation_record(conn, &record)?;
            Ok(id)
        }
        "memory_entity" => {
            let record: LinkSyncRecord =
                serde_json::from_value(raw.clone()).map_err(|e| GraphApplyError(e.to_string()))?;
            upsert_link_record(conn, &record)
        }
        other => Err(GraphApplyError(format!("unknown record_type: {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_aliases_unions_local_first_then_new_incoming_aliases_deduped() {
        let merged = merge_aliases(&["A".to_string()], &["A".to_string(), "B".to_string()]);
        assert_eq!(merged, vec!["A", "B"]);
    }
}
