//! Wire records, parsed and validated once for every backend.
//!
//! The reference does record-type dispatch, key validation and timestamp
//! canonicalisation inline in each `_upsert_*`, against a live Postgres. With
//! two backends here that would mean two copies of the fiddly part and one
//! place for them to disagree — so parsing happens here, produces a typed
//! [`Record`], and a [`crate::store::HubStore`] only ever sees something
//! already known to be well-formed.
//!
//! That split has a second benefit worth naming: a record rejected here never
//! reaches the database at all, so "malformed" and "storage failed" cannot be
//! confused in the push tally the way they can when validation lives inside
//! the transaction.

use crate::canon::{canon_ts, coerce_json_field};
use serde_json::Value;

/// A record that failed validation. Counted as `failed` in a push response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordError(pub String);

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A memory record, canonicalised and defaulted.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: Value,
    pub source: String,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
    pub capture_id: Option<String>,
    pub node_id: Option<String>,
    pub client: String,
    pub accessed_at: String,
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
    /// Whether the source memory is marked sensitive. Was missing from every
    /// column list in this crate — the hub's schema, wire columns, and this
    /// struct — so it was silently dropped on every push and every pull
    /// defaulted a synced memory to non-sensitive regardless of the source.
    /// Defaults to not-sensitive when absent, matching the sender's own
    /// backward-compatibility default for an older peer.
    pub sensitive: bool,
    /// A scheduled reminder time, if the memory carries one. Missing for the
    /// same reason `sensitive` was.
    pub remind_at: Option<String>,
}

/// An entity record. `aliases` is always a list of non-empty strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRecord {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub node_id: Option<String>,
}

/// A memory↔entity link. Immutable once written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRecord {
    pub memory_id: String,
    pub entity_id: String,
    pub created_at: String,
}

/// A typed entity-to-entity edge. Immutable once written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRelationRecord {
    pub id: String,
    pub subject_entity_id: String,
    pub relation: String,
    pub object_entity_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub node_id: Option<String>,
}

/// One record of any type.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Memory(Box<MemoryRecord>),
    Entity(EntityRecord),
    Link(LinkRecord),
    EntityRelation(EntityRelationRecord),
}

impl Record {
    /// The id the sender matches in `processed_ids`.
    ///
    /// Links have no `id` of their own on the wire, so the reference
    /// synthesises `memory_id|entity_id` — and the client matches on exactly
    /// that string, so it is not ours to prettify.
    pub fn wire_id(&self) -> String {
        match self {
            Record::Memory(m) => m.id.clone(),
            Record::Entity(e) => e.id.clone(),
            Record::Link(l) => format!("{}|{}", l.memory_id, l.entity_id),
            Record::EntityRelation(r) => r.id.clone(),
        }
    }
}

fn as_str(rec: &Value, key: &str) -> Option<String> {
    match rec.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// A field that may be present but empty. Distinct from [`as_str`] because
/// `""` and absent mean the same thing to the reference's `rec.get(k) or
/// default` idiom, but *not* to a nullable column that should stay null.
fn as_opt_str(rec: &Value, key: &str) -> Option<String> {
    match rec.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn or_default(rec: &Value, key: &str, default: &str) -> String {
    as_str(rec, key).unwrap_or_else(|| default.to_string())
}

fn as_f64(rec: &Value, key: &str, default: f64) -> f64 {
    match rec.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.parse().unwrap_or(default),
        _ => default,
    }
}

fn as_i64(rec: &Value, key: &str, default: i64) -> i64 {
    match rec.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(default),
        Some(Value::String(s)) => s.parse().unwrap_or(default),
        _ => default,
    }
}

/// Accept SQLite's integer booleans as well as real JSON booleans, mirroring
/// `crate::sync::record::de_sqlite_bool` in the core crate — a payload built
/// by that crate's own triggers carries `0`/`1`, one built by hand carries
/// `true`/`false`, and an absent or malformed value reads as `false` rather
/// than failing the whole record: `sensitive` is a property worth defaulting
/// safely, not one worth losing an otherwise-valid memory over.
fn as_bool(rec: &Value, key: &str, default: bool) -> bool {
    match rec.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => default,
    }
}

fn require(rec: &Value, keys: &[&str], what: &str) -> Result<(), RecordError> {
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| as_str(rec, k).is_none())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(RecordError(format!(
            "{what} missing required keys: {missing:?}"
        )))
    }
}

/// Parse one wire record, dispatching on `record_type` (absent = memory).
pub fn parse(rec: &Value) -> Result<Record, RecordError> {
    if !rec.is_object() {
        return Err(RecordError(format!(
            "record is not an object: {}",
            match rec {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "str",
                Value::Array(_) => "list",
                Value::Object(_) => unreachable!("handled above"),
            }
        )));
    }
    match rec.get("record_type").and_then(Value::as_str) {
        None | Some("") | Some("memory") => parse_memory(rec).map(Box::new).map(Record::Memory),
        Some("entity") => parse_entity(rec).map(Record::Entity),
        Some("memory_entity") => parse_link(rec).map(Record::Link),
        Some("entity_relation") => parse_entity_relation(rec).map(Record::EntityRelation),
        Some(other) => Err(RecordError(format!("unknown record_type: {other:?}"))),
    }
}

fn parse_memory(rec: &Value) -> Result<MemoryRecord, RecordError> {
    require(
        rec,
        &["id", "content", "created_at", "updated_at"],
        "record",
    )?;

    let created_at =
        canon_ts(&as_str(rec, "created_at").expect("required above")).map_err(RecordError)?;
    let updated_at =
        canon_ts(&as_str(rec, "updated_at").expect("required above")).map_err(RecordError)?;

    // The reference swallows a bad `accessed_at` and falls back to
    // `created_at` rather than failing the record -- an access timestamp is
    // decay bookkeeping, not content, and losing the memory over it would be a
    // bad trade.
    let accessed_at = as_str(rec, "accessed_at")
        .and_then(|raw| canon_ts(&raw).ok())
        .unwrap_or_else(|| created_at.clone());

    // `deleted_at` is different: it is the tombstone, so a malformed one must
    // fail the record rather than silently resurrect it.
    let deleted_at = match as_str(rec, "deleted_at") {
        Some(raw) => Some(canon_ts(&raw).map_err(RecordError)?),
        None => None,
    };

    Ok(MemoryRecord {
        id: as_str(rec, "id").expect("required above"),
        content: rec
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        category: or_default(rec, "category", "general"),
        tags: coerce_json_field(rec.get("tags"), Value::Array(vec![])),
        source: or_default(rec, "source", "manual"),
        metadata: coerce_json_field(rec.get("metadata"), Value::Object(Default::default())),
        created_at,
        updated_at,
        capture_id: as_opt_str(rec, "capture_id"),
        node_id: as_opt_str(rec, "node_id"),
        client: or_default(rec, "client", "unknown"),
        accessed_at,
        access_count: as_i64(rec, "access_count", 0),
        decay_rate: as_f64(rec, "decay_rate", 0.1),
        vitality: as_f64(rec, "vitality", 1.0),
        base_weight: as_f64(rec, "base_weight", 1.0),
        status: or_default(rec, "status", "active"),
        memory_type: or_default(rec, "memory_type", "unclassified"),
        source_capture_id: as_opt_str(rec, "source_capture_id"),
        subject: as_opt_str(rec, "subject"),
        predicate: as_opt_str(rec, "predicate"),
        object: as_opt_str(rec, "object"),
        superseded_by: as_opt_str(rec, "superseded_by"),
        deleted_at,
        sensitive: as_bool(rec, "sensitive", false),
        remind_at: as_opt_str(rec, "remind_at"),
    })
}

fn parse_entity(rec: &Value) -> Result<EntityRecord, RecordError> {
    require(
        rec,
        &["id", "name", "created_at", "updated_at"],
        "entity record",
    )?;

    // Non-string and empty aliases are dropped rather than rejected: the union
    // merge downstream is only meaningful over real names, and one junk entry
    // should not cost the entity its other aliases.
    let aliases_value = coerce_json_field(rec.get("aliases"), Value::Array(vec![]));
    let mut aliases: Vec<String> = Vec::new();
    if let Value::Array(items) = aliases_value {
        for item in items {
            if let Value::String(s) = item {
                if !s.is_empty() && !aliases.contains(&s) {
                    aliases.push(s);
                }
            }
        }
    }

    Ok(EntityRecord {
        id: as_str(rec, "id").expect("required above"),
        name: as_str(rec, "name").expect("required above"),
        kind: as_opt_str(rec, "kind"),
        aliases,
        created_at: canon_ts(&as_str(rec, "created_at").expect("required above"))
            .map_err(RecordError)?,
        updated_at: canon_ts(&as_str(rec, "updated_at").expect("required above"))
            .map_err(RecordError)?,
        node_id: as_opt_str(rec, "node_id"),
    })
}

fn parse_link(rec: &Value) -> Result<LinkRecord, RecordError> {
    require(
        rec,
        &["memory_id", "entity_id", "created_at"],
        "link record",
    )?;
    Ok(LinkRecord {
        memory_id: as_str(rec, "memory_id").expect("required above"),
        entity_id: as_str(rec, "entity_id").expect("required above"),
        created_at: canon_ts(&as_str(rec, "created_at").expect("required above"))
            .map_err(RecordError)?,
    })
}

fn parse_entity_relation(rec: &Value) -> Result<EntityRelationRecord, RecordError> {
    require(
        rec,
        &[
            "id",
            "subject_entity_id",
            "relation",
            "object_entity_id",
            "created_at",
        ],
        "entity_relation record",
    )?;
    let created_at =
        canon_ts(&as_str(rec, "created_at").expect("required above")).map_err(RecordError)?;
    // Relations are immutable, so `updated_at` is bookkeeping that falls back
    // to `created_at` rather than being required on the wire.
    let updated_at = match as_str(rec, "updated_at") {
        Some(raw) => canon_ts(&raw).map_err(RecordError)?,
        None => created_at.clone(),
    };
    Ok(EntityRelationRecord {
        id: as_str(rec, "id").expect("required above"),
        subject_entity_id: as_str(rec, "subject_entity_id").expect("required above"),
        relation: as_str(rec, "relation").expect("required above"),
        object_entity_id: as_str(rec, "object_entity_id").expect("required above"),
        created_at,
        updated_at,
        node_id: as_opt_str(rec, "node_id"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn memory() -> Value {
        json!({
            "id": "m1",
            "content": "hello",
            "created_at": "2026-08-05T10:00:00Z",
            "updated_at": "2026-08-05T11:00:00Z",
        })
    }

    #[test]
    fn a_record_with_no_record_type_is_a_memory() {
        assert!(matches!(parse(&memory()).unwrap(), Record::Memory(_)));
    }

    #[test]
    fn defaults_match_the_reference() {
        let Record::Memory(m) = parse(&memory()).unwrap() else {
            panic!("expected a memory");
        };
        assert_eq!(m.category, "general");
        assert_eq!(m.source, "manual");
        assert_eq!(m.client, "unknown");
        assert_eq!(m.status, "active");
        assert_eq!(m.memory_type, "unclassified");
        assert_eq!(m.access_count, 0);
        assert_eq!(m.decay_rate, 0.1);
        assert_eq!(m.vitality, 1.0);
        assert_eq!(m.base_weight, 1.0);
        assert_eq!(m.tags, json!([]));
        assert_eq!(m.metadata, json!({}));
    }

    #[test]
    fn timestamps_are_canonicalised_on_the_way_in() {
        let Record::Memory(m) = parse(&memory()).unwrap() else {
            panic!("expected a memory");
        };
        assert_eq!(m.created_at, "2026-08-05T10:00:00+00:00");
        assert_eq!(m.updated_at, "2026-08-05T11:00:00+00:00");
    }

    #[test]
    fn a_missing_accessed_at_falls_back_to_created_at() {
        let Record::Memory(m) = parse(&memory()).unwrap() else {
            panic!("expected a memory");
        };
        assert_eq!(m.accessed_at, m.created_at);
    }

    #[test]
    fn a_malformed_accessed_at_degrades_but_a_malformed_deleted_at_fails() {
        // The asymmetry is deliberate and worth pinning: losing an access
        // timestamp costs decay accuracy, losing a tombstone resurrects a
        // deleted memory.
        let mut rec = memory();
        rec["accessed_at"] = json!("not a date");
        assert!(parse(&rec).is_ok());

        let mut rec = memory();
        rec["deleted_at"] = json!("not a date");
        assert!(parse(&rec).is_err());
    }

    #[test]
    fn missing_required_keys_are_reported_by_name() {
        let mut rec = memory();
        rec.as_object_mut().unwrap().remove("content");
        let err = parse(&rec).unwrap_err();
        assert!(err.0.contains("content"), "{}", err.0);
    }

    #[test]
    fn an_unknown_record_type_is_rejected() {
        let mut rec = memory();
        rec["record_type"] = json!("wat");
        assert!(parse(&rec).unwrap_err().0.contains("wat"));
    }

    #[test]
    fn a_non_object_record_is_rejected_by_type_name() {
        assert!(parse(&json!([1, 2, 3])).unwrap_err().0.contains("list"));
        assert!(parse(&json!("x")).unwrap_err().0.contains("str"));
    }

    #[test]
    fn a_links_wire_id_is_the_synthetic_composite() {
        let rec = json!({
            "record_type": "memory_entity",
            "memory_id": "m1",
            "entity_id": "e1",
            "created_at": "2026-08-05T10:00:00Z",
        });
        assert_eq!(parse(&rec).unwrap().wire_id(), "m1|e1");
    }

    #[test]
    fn entity_aliases_drop_junk_and_deduplicate() {
        let rec = json!({
            "record_type": "entity",
            "id": "e1",
            "name": "Ada",
            "aliases": ["Ada", "", 7, "Ada", "Lovelace", null],
            "created_at": "2026-08-05T10:00:00Z",
            "updated_at": "2026-08-05T10:00:00Z",
        });
        let Record::Entity(e) = parse(&rec).unwrap() else {
            panic!("expected an entity");
        };
        assert_eq!(e.aliases, vec!["Ada".to_string(), "Lovelace".to_string()]);
    }

    #[test]
    fn a_relation_without_updated_at_borrows_created_at() {
        let rec = json!({
            "record_type": "entity_relation",
            "id": "r1",
            "subject_entity_id": "e1",
            "relation": "knows",
            "object_entity_id": "e2",
            "created_at": "2026-08-05T10:00:00Z",
        });
        let Record::EntityRelation(r) = parse(&rec).unwrap() else {
            panic!("expected a relation");
        };
        assert_eq!(r.updated_at, r.created_at);
    }
}
