//! The hub's own record types for the engine (ADR-0021, decision 2), one
//! per table, each carrying every column the retired SQLite and Postgres
//! stores held.
//!
//! # The shape the engine asks for
//!
//! A [`GenericMmapStore`](rusty_multimodal_db_engine::generic::GenericMmapStore)
//! keeps one equality-indexed field and one durable scannable field per
//! record, and [`Ordered`](rusty_multimodal_db_engine::generic::store::Ordered)
//! layers add sorted indexes. Per table:
//!
//! | Table | Scannable slot | Sort orders |
//! |---|---|---|
//! | memories | `hub_seq` | `hub_seq`; `(updated_at, id)` |
//! | entities | `updated_at` µs | `(updated_at, id)` |
//! | memory_entities | `created_at` µs | `(created_at, memory_id\|entity_id)` |
//! | entity_relations | `created_at` µs | `(created_at, id)` |
//!
//! The equality index is keyed on the engine id itself. The hub looks
//! nothing up by value, and a per-record bucket keeps the engine's
//! replace and delete O(1) (a shared bucket is scanned on every delete).
//!
//! # Encoding
//!
//! Records are bincode on disk, which cannot decode a `serde_json::Value`,
//! so JSON columns (`tags`, `metadata`, `aliases`) are kept as JSON text. Each schema tag carries a layout
//! version: the engine refuses a blob written under another tag, so a
//! change to any of these structs needs a new tag and a hand-written
//! conversion (ADR-0021, Consequences).

use super::keys::{self, IdKey, LinkKey};
use crate::record::{EntityRecord, EntityRelationRecord, LinkRecord, MemoryRecord};
use crate::store::StoreResult;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, OrderedField, Record, ScannableField, SchemaTag,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// Marker: the equality index, on the engine id.
pub struct ByEngineId;
/// Marker: a memory's `hub_seq`, both its durable slot and a sort order.
pub struct Seq;
/// Marker: a timestamp in µs, the durable slot of the other three tables.
pub struct Micros;
/// Marker: the `(timestamp, id)` keyset order each table pages by.
pub struct Keyset;

/// Implement [`Record`] and the engine-id equality index for a row type.
macro_rules! engine_row {
    ($row:ty, $tag:literal) => {
        impl Record for $row {
            type Id = Uuid;
            fn id(&self) -> Uuid {
                self.engine_id
            }
        }

        impl SchemaTag for $row {
            const SCHEMA_TAG: &'static str = $tag;
        }

        impl IndexedField<ByEngineId> for $row {
            type IndexValue = Uuid;
            fn indexed_value(&self) -> &Uuid {
                &self.engine_id
            }
        }
    };
}

/// Implement the µs slot and the `(µs, key)` keyset order for a row type.
macro_rules! timestamp_ordered {
    ($row:ty, $micros:ident, $key_ty:ty, $key:expr) => {
        impl ScannableField<Micros> for $row {
            type ScanValue = i64;
            fn scannable_value(&self) -> i64 {
                self.$micros
            }
            fn set_scannable_value(&mut self, value: i64) {
                self.$micros = value;
            }
        }

        impl OrderedField<Keyset> for $row {
            type Key = (i64, $key_ty);
            fn order_key(&self) -> Self::Key {
                (self.$micros, $key(self))
            }
        }
    };
}

/// Parse a JSON text column, degrading to `default` if it is not JSON.
fn json_text(raw: &str, default: Value) -> Value {
    serde_json::from_str(raw).unwrap_or(default)
}

fn to_json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

/// One `memories` row: all 28 columns of the retired SQL stores' table, the
/// engine id, and `updated_at` in µs for the keyset order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    pub engine_id: Uuid,
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: String,
    pub source: String,
    pub metadata: String,
    pub created_at: String,
    pub updated_at: String,
    pub updated_at_us: i64,
    pub capture_id: Option<String>,
    pub node_id: Option<String>,
    pub client: String,
    pub accessed_at: Option<String>,
    pub access_count: i64,
    pub decay_rate: f64,
    pub vitality: f64,
    pub base_weight: f64,
    pub status: String,
    pub memory_type: String,
    pub source_capture_id: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub superseded_by: Option<String>,
    pub deleted_at: Option<String>,
    pub origin_node: Option<String>,
    pub hub_seq: i64,
    pub sensitive: bool,
    pub remind_at: Option<String>,
}

engine_row!(MemoryRow, "rusty_remind_me::hub::MemoryRow@1");

impl ScannableField<Seq> for MemoryRow {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.hub_seq
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.hub_seq = value;
    }
}

impl OrderedField<Seq> for MemoryRow {
    type Key = i64;
    fn order_key(&self) -> i64 {
        self.hub_seq
    }
}

impl OrderedField<Keyset> for MemoryRow {
    type Key = (i64, IdKey);
    fn order_key(&self) -> Self::Key {
        (self.updated_at_us, keys::id_key(&self.id))
    }
}

impl MemoryRow {
    /// The row a validated wire record becomes, stamped with `hub_seq`.
    pub fn new(m: &MemoryRecord, origin: Option<&str>, hub_seq: i64) -> StoreResult<Self> {
        Ok(Self {
            engine_id: keys::engine_id(&m.id),
            id: m.id.clone(),
            content: m.content.clone(),
            category: m.category.clone(),
            tags: to_json_text(&m.tags),
            source: m.source.clone(),
            metadata: to_json_text(&m.metadata),
            created_at: m.created_at.clone(),
            updated_at: m.updated_at.clone(),
            updated_at_us: keys::micros(&m.updated_at)?,
            capture_id: m.capture_id.clone(),
            node_id: m.node_id.clone(),
            client: m.client.clone(),
            accessed_at: Some(m.accessed_at.clone()),
            access_count: m.access_count,
            decay_rate: m.decay_rate,
            vitality: m.vitality,
            base_weight: m.base_weight,
            status: m.status.clone(),
            memory_type: m.memory_type.clone(),
            source_capture_id: m.source_capture_id.clone(),
            subject: m.subject.clone(),
            predicate: m.predicate.clone(),
            object: m.object.clone(),
            superseded_by: m.superseded_by.clone(),
            deleted_at: m.deleted_at.clone(),
            origin_node: origin.map(str::to_string),
            hub_seq,
            sensitive: m.sensitive,
            remind_at: m.remind_at.clone(),
        })
    }

    /// The wire form, key for key what the retired SQLite store's pull
    /// returned.
    pub fn to_wire(&self) -> Value {
        json!({
            "id": self.id,
            "content": self.content,
            "category": self.category,
            "tags": json_text(&self.tags, json!([])),
            "source": self.source,
            "metadata": json_text(&self.metadata, json!({})),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "capture_id": self.capture_id,
            "node_id": self.node_id,
            "client": self.client,
            "accessed_at": self.accessed_at,
            "access_count": self.access_count,
            "decay_rate": self.decay_rate,
            "vitality": self.vitality,
            "base_weight": self.base_weight,
            "status": self.status,
            "memory_type": self.memory_type,
            "source_capture_id": self.source_capture_id,
            "subject": self.subject,
            "predicate": self.predicate,
            "object": self.object,
            "superseded_by": self.superseded_by,
            "deleted_at": self.deleted_at,
            "hub_seq": self.hub_seq,
            "sensitive": self.sensitive,
            "remind_at": self.remind_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// One `entities` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRow {
    pub engine_id: Uuid,
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub updated_at_us: i64,
    pub node_id: Option<String>,
    pub origin_node: Option<String>,
}

engine_row!(EntityRow, "rusty_remind_me::hub::EntityRow@1");
timestamp_ordered!(EntityRow, updated_at_us, IdKey, |r: &EntityRow| {
    keys::id_key(&r.id)
});

impl EntityRow {
    pub fn new(e: &EntityRecord, origin: Option<&str>) -> StoreResult<Self> {
        Ok(Self {
            engine_id: keys::engine_id(&e.id),
            id: e.id.clone(),
            name: e.name.clone(),
            kind: e.kind.clone(),
            aliases: e.aliases.clone(),
            created_at: e.created_at.clone(),
            updated_at: e.updated_at.clone(),
            updated_at_us: keys::micros(&e.updated_at)?,
            node_id: e.node_id.clone(),
            origin_node: origin.map(str::to_string),
        })
    }

    pub fn to_wire(&self) -> Value {
        json!({
            "record_type": "entity",
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "aliases": self.aliases,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "node_id": self.node_id,
        })
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// One `memory_entities` row. A table of its own rather than an engine
/// edge, so it keeps `created_at` and may arrive before its memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRow {
    pub engine_id: Uuid,
    pub memory_id: String,
    pub entity_id: String,
    pub created_at: String,
    pub created_at_us: i64,
}

engine_row!(LinkRow, "rusty_remind_me::hub::LinkRow@1");
timestamp_ordered!(LinkRow, created_at_us, LinkKey, |r: &LinkRow| {
    keys::link_key(&r.synthetic_id())
});

impl LinkRow {
    pub fn new(l: &LinkRecord) -> StoreResult<Self> {
        Ok(Self {
            engine_id: keys::link_engine_id(&l.memory_id, &l.entity_id),
            memory_id: l.memory_id.clone(),
            entity_id: l.entity_id.clone(),
            created_at: l.created_at.clone(),
            created_at_us: keys::micros(&l.created_at)?,
        })
    }

    /// The wire id a client pages links by.
    pub fn synthetic_id(&self) -> String {
        format!("{}|{}", self.memory_id, self.entity_id)
    }

    pub fn to_wire(&self) -> Value {
        json!({
            "record_type": "memory_entity",
            "id": self.synthetic_id(),
            "memory_id": self.memory_id,
            "entity_id": self.entity_id,
            "created_at": self.created_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Entity relations
// ---------------------------------------------------------------------------

/// One `entity_relations` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationRow {
    pub engine_id: Uuid,
    pub id: String,
    pub subject_entity_id: String,
    pub relation: String,
    pub object_entity_id: String,
    pub created_at: String,
    pub created_at_us: i64,
    pub updated_at: String,
    pub node_id: Option<String>,
    pub origin_node: Option<String>,
}

engine_row!(RelationRow, "rusty_remind_me::hub::RelationRow@1");
timestamp_ordered!(RelationRow, created_at_us, IdKey, |r: &RelationRow| {
    keys::id_key(&r.id)
});

impl RelationRow {
    pub fn new(r: &EntityRelationRecord, origin: Option<&str>) -> StoreResult<Self> {
        Ok(Self {
            engine_id: keys::engine_id(&r.id),
            id: r.id.clone(),
            subject_entity_id: r.subject_entity_id.clone(),
            relation: r.relation.clone(),
            object_entity_id: r.object_entity_id.clone(),
            created_at: r.created_at.clone(),
            created_at_us: keys::micros(&r.created_at)?,
            updated_at: r.updated_at.clone(),
            node_id: r.node_id.clone(),
            origin_node: origin.map(str::to_string),
        })
    }

    pub fn to_wire(&self) -> Value {
        json!({
            "record_type": "entity_relation",
            "id": self.id,
            "subject_entity_id": self.subject_entity_id,
            "relation": self.relation,
            "object_entity_id": self.object_entity_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "node_id": self.node_id,
        })
    }
}
