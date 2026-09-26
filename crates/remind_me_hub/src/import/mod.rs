//! Read a whole hub out of the retired SQLite or Postgres store, for the copy tool
//! (`rusty-remind-me-hub-copy`, ADR-0021 phase 3).
//!
//! Each reader turns every row into a JSON object keyed by column name and
//! hands it to [`crate::record::parse`], the validation and defaulting a
//! push goes through. A row reads exactly as it would arrive on the wire:
//! timestamps canonicalised, missing columns defaulted (a legacy database
//! lacks many). Nothing is written to the source: SQLite is opened
//! read-only, Postgres read in a read-only transaction, and a legacy
//! Postgres schema is read as it stands rather than migrated first.
//!
//! The two columns the wire never carries, `origin_node` and `hub_seq`,
//! are read beside the record and carried over exactly.

pub mod sqlite;

#[cfg(feature = "postgres-import")]
pub mod postgres;

use crate::record::{self, Record};
use crate::store::multimodal::snapshot::{assign_missing_seq, Rejected, Snapshot};
use crate::store::{
    GraphPullQuery, HubStore, PullCursor, PullQuery, StoreError, StoreResult, MAX_PULL_LIMIT,
};
use serde_json::{Map, Value};

/// A source hub's tables, one JSON object per row.
#[derive(Debug, Default)]
pub struct RawTables {
    pub memories: Vec<Map<String, Value>>,
    pub entities: Vec<Map<String, Value>>,
    pub links: Vec<Map<String, Value>>,
    pub relations: Vec<Map<String, Value>>,
    /// See [`Snapshot::seq_high_water`].
    pub seq_high_water: i64,
}

/// Memory columns a push defaults when empty, but which a stored row keeps
/// verbatim, empty string included (`record::parse` would turn a stored
/// `""` category into `"general"`).
const VERBATIM_TEXT: [&str; 5] = ["category", "source", "client", "status", "memory_type"];

impl RawTables {
    /// Parse every row. A row that does not parse is kept out and reported
    /// in [`Snapshot::unreadable`], never dropped silently.
    pub fn into_snapshot(self) -> Snapshot {
        let mut unreadable = Vec::new();

        let mut memories = Vec::new();
        for row in self.memories {
            match memory(&row) {
                Ok(parsed) => memories.push(parsed),
                Err(reason) => unreadable.push(rejected("memories", &row, "id", reason)),
            }
        }

        let mut entities = Vec::new();
        for row in self.entities {
            match parse_as(&row, "entity") {
                Ok(Record::Entity(e)) => entities.push((e, text(&row, "origin_node"))),
                Ok(_) => unreachable!("parsed as an entity"),
                Err(reason) => unreadable.push(rejected("entities", &row, "id", reason)),
            }
        }

        let mut links = Vec::new();
        for row in self.links {
            match parse_as(&row, "memory_entity") {
                Ok(Record::Link(l)) => links.push(l),
                Ok(_) => unreachable!("parsed as a link"),
                Err(reason) => {
                    unreadable.push(rejected("memory_entities", &row, "memory_id", reason))
                }
            }
        }

        let mut relations = Vec::new();
        for row in self.relations {
            match parse_as(&row, "entity_relation") {
                Ok(Record::EntityRelation(r)) => relations.push((r, text(&row, "origin_node"))),
                Ok(_) => unreachable!("parsed as a relation"),
                Err(reason) => unreadable.push(rejected("entity_relations", &row, "id", reason)),
            }
        }

        Snapshot {
            memories: assign_missing_seq(memories, self.seq_high_water),
            entities,
            links,
            relations,
            seq_high_water: self.seq_high_water,
            unreadable,
        }
    }
}

fn text(row: &Map<String, Value>, column: &str) -> Option<String> {
    row.get(column).and_then(Value::as_str).map(str::to_string)
}

fn parse_as(row: &Map<String, Value>, record_type: &str) -> Result<Record, String> {
    let mut row = row.clone();
    row.insert("record_type".into(), Value::String(record_type.into()));
    record::parse(&Value::Object(row)).map_err(|e| e.0)
}

type ParsedMemory = (record::MemoryRecord, Option<String>, Option<i64>);

fn memory(row: &Map<String, Value>) -> Result<ParsedMemory, String> {
    let Record::Memory(mut m) = parse_as(row, "memory")? else {
        unreachable!("parsed as a memory");
    };
    for column in VERBATIM_TEXT {
        if let Some(Value::String(stored)) = row.get(column) {
            let field = match column {
                "category" => &mut m.category,
                "source" => &mut m.source,
                "client" => &mut m.client,
                "status" => &mut m.status,
                _ => &mut m.memory_type,
            };
            field.clone_from(stored);
        }
    }
    let hub_seq = row.get("hub_seq").and_then(Value::as_i64);
    Ok((*m, text(row, "origin_node"), hub_seq))
}

fn rejected(table: &'static str, row: &Map<String, Value>, key: &str, reason: String) -> Rejected {
    Rejected {
        table,
        id: row
            .get(key)
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .unwrap_or_default(),
        reason,
    }
}

/// Read everything back from `store` and require it to hold exactly the
/// snapshot's rows: every memory with its `hub_seq` and `updated_at`, and
/// every entity, link and relation id.
///
/// # Errors
///
/// A [`StoreError`] naming the first table that differs, or any error
/// reading the store.
pub fn verify(snapshot: &Snapshot, store: &dyn HubStore) -> StoreResult<()> {
    let mut want: Vec<(i64, String, String)> = snapshot
        .memories
        .iter()
        .map(|m| (m.hub_seq, m.record.id.clone(), m.record.updated_at.clone()))
        .collect();
    want.sort();
    let got: Vec<(i64, String, String)> = pull_all(|limit, last: Option<&Value>| {
        let seq = last.and_then(|m| m["hub_seq"].as_i64()).unwrap_or(0);
        store.pull_memories(&memory_query(PullCursor::Seq(seq), limit))
    })?
    .iter()
    .map(|m| {
        (
            field_i64(m, "hub_seq"),
            field(m, "id"),
            field(m, "updated_at"),
        )
    })
    .collect();
    same("memories (hub_seq, id, updated_at)", want, got)?;

    let entities = pull_all(|limit, last: Option<&Value>| {
        let cursor = match last {
            None => PullCursor::Since(crate::EPOCH.to_string()),
            Some(e) => PullCursor::Keyset {
                since: field(e, "updated_at"),
                since_id: field(e, "id"),
            },
        };
        store.pull_entities(&memory_query(cursor, limit))
    })?;
    same(
        "entities",
        sorted(snapshot.entities.iter().map(|(e, _)| e.id.clone())),
        sorted(entities.iter().map(|e| field(e, "id"))),
    )?;

    let graph = |pull: &dyn Fn(&GraphPullQuery) -> StoreResult<Vec<Value>>| {
        pull_all(|limit, last: Option<&Value>| {
            let (since, since_id) = last.map_or_else(
                || (crate::EPOCH.to_string(), String::new()),
                |r| (field(r, "created_at"), field(r, "id")),
            );
            pull(&GraphPullQuery {
                since,
                since_id,
                limit,
            })
        })
    };
    let links = graph(&|q| store.pull_links(q))?;
    same(
        "memory_entities",
        sorted(
            snapshot
                .links
                .iter()
                .map(|l| format!("{}|{}", l.memory_id, l.entity_id)),
        ),
        sorted(links.iter().map(|l| field(l, "id"))),
    )?;
    let relations = graph(&|q| store.pull_entity_relations(q))?;
    same(
        "entity_relations",
        sorted(snapshot.relations.iter().map(|(r, _)| r.id.clone())),
        sorted(relations.iter().map(|r| field(r, "id"))),
    )
}

fn memory_query(cursor: PullCursor, limit: usize) -> PullQuery {
    PullQuery {
        cursor,
        exclude_node: None,
        full: true,
        limit,
    }
}

/// Every page of one pull, each asked for after the last row of the one
/// before.
fn pull_all(
    mut page: impl FnMut(usize, Option<&Value>) -> StoreResult<Vec<Value>>,
) -> StoreResult<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    loop {
        let rows = page(MAX_PULL_LIMIT, out.last())?;
        let done = rows.len() < MAX_PULL_LIMIT;
        out.extend(rows);
        if done {
            return Ok(out);
        }
    }
}

fn field(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or_default().to_string()
}

fn field_i64(row: &Value, key: &str) -> i64 {
    row[key].as_i64().unwrap_or_default()
}

fn sorted(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut items: Vec<String> = items.collect();
    items.sort();
    items
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, want: Vec<T>, got: Vec<T>) -> StoreResult<()> {
    if want == got {
        return Ok(());
    }
    let first = want
        .iter()
        .zip(&got)
        .position(|(w, g)| w != g)
        .unwrap_or_else(|| want.len().min(got.len()));
    Err(StoreError(format!(
        "{what}: the copy holds {} rows where the source had {}; first difference at \
         row {first}: source {:?}, copy {:?}",
        got.len(),
        want.len(),
        want.get(first),
        got.get(first)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_row_keeps_its_hub_seq_origin_and_stored_empty_text() {
        let raw = RawTables {
            memories: vec![obj(json!({
                "id": "m1", "content": "x", "category": "",
                "created_at": "2026-08-01T00:00:00+00:00",
                "updated_at": "2026-08-02T00:00:00+00:00",
                "origin_node": "node-a", "hub_seq": 41, "sensitive": 1,
            }))],
            ..RawTables::default()
        };
        let snap = raw.into_snapshot();
        let m = &snap.memories[0];
        assert_eq!(m.hub_seq, 41);
        assert_eq!(m.origin_node.as_deref(), Some("node-a"));
        assert_eq!(
            m.record.category, "",
            "a stored empty category is not defaulted"
        );
        assert!(m.record.sensitive);
    }

    #[test]
    fn a_legacy_row_is_defaulted_and_numbered_as_a_migration_would() {
        // Postgres renders a TIMESTAMPTZ as ISO text with an offset and a
        // trimmed fraction; the row lacks every column added since.
        let raw = RawTables {
            memories: vec![
                obj(json!({"id": "b", "content": "x",
                    "created_at": "2026-08-01T00:00:00+02:00",
                    "updated_at": "2026-08-02T00:00:00.5+00:00"})),
                obj(json!({"id": "a", "content": "y",
                    "created_at": "2026-08-01T00:00:00+00:00",
                    "updated_at": "2026-08-01T00:00:00+00:00"})),
                obj(json!({"id": "c", "content": "z", "hub_seq": 7,
                    "created_at": "2026-08-01T00:00:00+00:00",
                    "updated_at": "2026-07-01T00:00:00+00:00"})),
            ],
            seq_high_water: 9,
            ..RawTables::default()
        };
        let snap = raw.into_snapshot();
        let seq = |id: &str| snap.memories.iter().find(|m| m.record.id == id).unwrap();
        assert_eq!(seq("c").hub_seq, 7, "a stored hub_seq is kept");
        // Missing ones go above the high-water mark, oldest updated_at first.
        assert_eq!(seq("a").hub_seq, 10);
        assert_eq!(seq("b").hub_seq, 11);
        assert_eq!(
            seq("b").record.updated_at,
            "2026-08-02T00:00:00.500000+00:00"
        );
        assert_eq!(seq("b").record.created_at, "2026-07-31T22:00:00+00:00");
        assert_eq!(seq("b").record.status, "active");
        assert_eq!(seq("b").record.accessed_at, seq("b").record.created_at);
        assert_eq!(snap.seq_floor(), 11);
    }

    #[test]
    fn an_unreadable_row_is_reported_not_dropped() {
        let raw = RawTables {
            memories: vec![obj(json!({"id": "m1", "content": "x",
                "created_at": "yesterday", "updated_at": "2026-08-01T00:00:00Z"}))],
            ..RawTables::default()
        };
        let snap = raw.into_snapshot();
        assert!(snap.memories.is_empty());
        assert_eq!(snap.validate().len(), 1);
        assert_eq!(snap.validate()[0].id, "m1");
    }
}
