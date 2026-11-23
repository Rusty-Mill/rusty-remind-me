//! D1 and D2 (spec § Datasets): the real store, loaded into a candidate
//! through the ports, and grown with synthetic memories.
//!
//! D1 comes from a copy of the newest remindme backup (an engine directory,
//! `manual-*.engine`). The copy is opened as an engine store and every
//! memory, chunk vector, entity, memory-entity link, relation, reference
//! and the embedding identity are written into the candidate through its
//! ports, so each candidate stores the same rows its own way. Dialog
//! bodies follow R3's layout (D50, D51): the body goes to the blob store
//! and the row keeps the head and a link. The backup itself is never
//! opened, only a private copy (refused if it holds anything but plain
//! files and directories), removed on every exit.
//!
//! The copy is opened as any node opens its store, which applies that
//! open's normal housekeeping (the sync flag, the outbox prune, emptying
//! tombstones' text); what is loaded is that opened, normalized copy, not
//! the untouched backup. Before any timing, a parity check compares the
//! candidate with that copy row by row (so it shows the load is faithful
//! to the copy, not that the copy equals the backup byte for byte):
//! every memory (a dialog with its body stashed), every stored entity,
//! link, relation and chunk row, every reference and the embedding
//! identity. Any difference fails the run.
//!
//! Queries on D1 are real but serve timing only, not relevance (G2): a
//! sampled memory's leading words, with that memory's stored vector as
//! the query vector, so no model is needed and the vectors searched have
//! the store's own distribution. A text two memories share is used once,
//! so each query text names exactly one vector.

use remind_me_core::db::derived::{rebuild_indexes, Origin};
use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::entities::{Entities, RelationRow};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::references::{NewReference, References};
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::Store;
use remind_me_core::models::Memory;
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use serde_json::{Map, Value};
use std::path::Path;

use super::bodies::Bodies;
use super::package::{copy_dir, row_hash};
use super::{Result, DIALOG};

/// Memories read from the source per batch.
const BATCH: usize = 500;
/// Words of a memory's content that make its query text.
const QUERY_WORDS: usize = 6;
/// The memory count D2 grows D1 to (3x headroom).
pub const D2_TOTAL: usize = 75_000;

/// What a loaded store holds that the workloads need.
pub struct Loaded {
    /// Live memory ids (tombstones are loaded but left out).
    pub ids: Vec<String>,
    /// Entity ids.
    pub entities: Vec<String>,
    /// Rows written, by kind, for the report.
    pub counts: Map<String, Value>,
    /// The parity check's result.
    pub parity: Value,
    /// The embedding identity loaded, which no run may change.
    pub identity: Vec<(String, String)>,
}

/// Load the backup at `backup` into `target`, by way of a copy at
/// `scratch` (which must not exist and is removed afterwards).
pub fn load(backup: &Path, scratch: &Path, target: &Store<'_>, bodies: &Bodies) -> Result<Loaded> {
    if !backup.is_dir() {
        return Err(format!("D1 backup {} is not a directory", backup.display()).into());
    }
    if scratch.exists() {
        return Err(format!("D1 copy {} already exists", scratch.display()).into());
    }
    let _scratch = Scratch(scratch);
    copy_dir(backup, scratch, &[])?;
    let source = Database::open_backend(Box::new(EngineTables::open(scratch)?), None)?;
    let store = source.store();
    let mut loaded = copy_rows(&store, target, bodies)?;
    rebuild_indexes(target)?;
    loaded.parity = parity(&store, target, bodies)?;
    loaded.identity = Vectors::new(&store).meta()?;
    Ok(loaded)
}

/// The private copy, removed when the load ends, however it ends.
struct Scratch<'p>(&'p Path);

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        if self.0.exists() {
            if let Err(e) = std::fs::remove_dir_all(self.0) {
                eprintln!(
                    "store_bench: cannot remove the D1 copy {}: {e}",
                    self.0.display()
                );
            }
        }
    }
}

/// Compare `target` with `source` row by row: each memory as the loader
/// writes it, each stored graph and chunk row, each reference and the
/// embedding identity. The counts compared, or an error naming what
/// differs.
fn parity(source: &Store<'_>, target: &Store<'_>, bodies: &Bodies) -> Result<Value> {
    let mut differ = Vec::new();
    let ids = testing::memory_ids(source)?;
    if testing::memory_ids(target)? != ids {
        differ.push("memory ids".to_string());
    }
    let mut memories = 0;
    for chunk in ids.chunks(BATCH) {
        let expected = Memories::new(source).get_many(chunk)?;
        let found = Memories::new(target).get_many(chunk)?;
        for (want, got) in expected.into_iter().zip(found) {
            if stashed(row(want), bodies)? != row(got) {
                memories += 1;
            }
        }
    }
    if memories > 0 {
        differ.push(format!("{memories} memories"));
    }
    let mut counts = Map::new();
    for table in [
        Table::Entities,
        Table::MemoryEntities,
        Table::EntityRelations,
        Table::VecChunks,
    ] {
        let rows = |store: &Store<'_>| -> Result<Vec<String>> {
            let mut hashes: Vec<String> = testing::stored_rows(store, table)?
                .iter()
                .map(row_hash)
                .collect();
            hashes.sort();
            Ok(hashes)
        };
        let want = rows(source)?;
        if rows(target)? != want {
            differ.push(format!("{table:?} rows"));
        }
        counts.insert(format!("{table:?}"), want.len().into());
    }
    let references = |store: &Store<'_>| -> Result<Vec<String>> {
        let mut all = Vec::new();
        for chunk in ids.chunks(BATCH) {
            for r in References::new(store).for_memories(chunk)? {
                all.push(format!(
                    "{}|{}|{}|{}|{:?}|{}",
                    r.id, r.memory_id, r.kind, r.value, r.label, r.created_at
                ));
            }
        }
        all.sort();
        Ok(all)
    };
    if references(target)? != references(source)? {
        differ.push("references".into());
    }
    if Vectors::new(target).meta()? != Vectors::new(source).meta()? {
        differ.push("embedding identity".into());
    }
    if !differ.is_empty() {
        return Err(format!(
            "D1 parity: the candidate differs from the opened copy in {}",
            differ.join(", ")
        )
        .into());
    }
    counts.insert("memories".into(), ids.len().into());
    counts.insert("matched".into(), true.into());
    counts.insert(
        "against".into(),
        "the opened copy of the backup, after normal startup's housekeeping; not the untouched backup"
            .into(),
    );
    Ok(Value::Object(counts))
}

/// Every row D1 covers, from `source` into `target`.
fn copy_rows(source: &Store<'_>, target: &Store<'_>, bodies: &Bodies) -> Result<Loaded> {
    let mut counts = Map::new();
    let mut count = |kind: &str, n: usize| {
        counts.insert(kind.into(), n.into());
    };

    let all = testing::memory_ids(source)?;
    let mut ids = Vec::with_capacity(all.len());
    // The embedding identity is stamped with the store's newest write,
    // which is when the source last held it.
    let mut newest = String::new();
    for chunk in all.chunks(BATCH) {
        for memory in Memories::new(source).get_many(chunk)? {
            if memory.deleted_at.is_none() {
                ids.push(memory.id.clone());
            }
            newest = newest.max(memory.updated_at.clone());
            Memories::new(target).insert(&stashed(row(memory), bodies)?)?;
        }
    }
    count("memories", all.len());

    let chunks = testing::stored_rows(source, Table::VecChunks)?;
    for chunk in &chunks {
        let ix = usize::try_from(chunk["chunk_ix"].as_u64().ok_or("chunk_ix")?)?;
        Vectors::new(target).put(text(chunk, "memory_id")?, ix, &bytes(chunk)?)?;
    }
    count("vectors", chunks.len());
    for (key, value) in Vectors::new(source).meta()? {
        Vectors::new(target).set_meta(&key, &value, &newest)?;
    }

    let (from, to) = (Entities::new(source), Entities::new(target));
    let nodes: std::collections::HashMap<String, Option<String>> =
        testing::stored_rows(source, Table::Entities)?
            .into_iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or("").to_string(),
                    r["node_id"].as_str().map(String::from),
                )
            })
            .collect();
    let mut entities = Vec::new();
    for entity in from.all_oldest_first()? {
        let node = nodes.get(&entity.id).cloned().flatten();
        to.insert(&entity, node.as_deref())?;
        entities.push(entity.id);
    }
    count("entities", entities.len());
    let links = from.links_oldest_first()?;
    for (memory, entity, created_at) in &links {
        to.link(memory, entity, created_at, Origin::Sync)?;
    }
    count("links", links.len());
    let relations = testing::stored_rows(source, Table::EntityRelations)?;
    for r in &relations {
        let row = RelationRow {
            id: text(r, "id")?,
            subject_entity_id: text(r, "subject_entity_id")?,
            relation: text(r, "relation")?,
            object_entity_id: text(r, "object_entity_id")?,
            created_at: text(r, "created_at")?,
            updated_at: text(r, "updated_at")?,
            node_id: r["node_id"].as_str(),
        };
        to.insert_relation_or_ignore(&row, Origin::Sync)?;
    }
    count("relations", relations.len());

    let mut references = 0;
    for chunk in all.chunks(BATCH) {
        for r in References::new(source).for_memories(chunk)? {
            References::new(target).insert(&NewReference {
                id: r.id,
                memory_id: r.memory_id,
                kind: r.kind,
                value: r.value,
                label: r.label,
                created_at: r.created_at,
            })?;
            references += 1;
        }
    }
    count("references", references);
    Ok(Loaded {
        ids,
        entities,
        counts,
        parity: Value::Null,
        identity: Vec::new(),
    })
}

/// A dialog's row with its body moved to `bodies` (D50): the head as its
/// content, and the link and body length added to its metadata. Any other
/// row as it is.
fn stashed(mut row: NewMemory, bodies: &Bodies) -> Result<NewMemory> {
    if row.category != DIALOG {
        return Ok(row);
    }
    let stash = bodies.stash(&row.content)?;
    let mut metadata = match row.metadata {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if let Value::Object(link) = stash.metadata() {
        metadata.extend(link);
    }
    row.metadata = Value::Object(metadata);
    row.content = stash.head;
    Ok(row)
}

/// A read memory as the row a writer supplies: every column kept.
fn row(m: Memory) -> NewMemory {
    let defaults = NewMemory::new(m.id.clone(), String::new(), &m.created_at);
    NewMemory {
        id: m.id,
        content: m.content,
        category: m.category,
        tags: m.tags,
        source: m.source,
        metadata: m.metadata,
        created_at: m.created_at,
        updated_at: m.updated_at,
        capture_id: m.capture_id,
        subject: m.subject,
        predicate: m.predicate,
        object: m.object,
        superseded_by: m.superseded_by,
        decay_rate: m.decay_rate,
        vitality: m.vitality,
        base_weight: m.base_weight,
        access_count: m.access_count,
        accessed_at: Some(m.accessed_at),
        doc_id: m.doc_id,
        chunk_index: m.chunk_index,
        remind_at: m.remind_at,
        sensitive: m.sensitive,
        memory_type: m.memory_type.unwrap_or(defaults.memory_type),
        status: m.status.unwrap_or(defaults.status),
        node_id: m.node_id,
        client: m.client.unwrap_or(defaults.client),
        source_capture_id: m.source_capture_id,
        deleted_at: m.deleted_at,
        project: m.project,
        session_id: m.session_id,
        git_remote: m.git_remote,
        git_branch: m.git_branch,
        git_sha: m.git_sha,
        cwd: m.cwd,
        valid_from: m.valid_from,
        valid_until: m.valid_until,
        confidence: m.confidence,
        verified_at: m.verified_at,
        outcome: m.outcome,
        written_by: m.written_by,
        capture_method: m.capture_method,
    }
}

/// `count` real queries from `store`: for memories picked by `pick` from
/// `ids`, the leading words of the content and the first chunk vector.
/// A memory with no vector or no words is passed over.
pub fn queries(
    store: &Store<'_>,
    ids: &[String],
    count: usize,
    mut pick: impl FnMut(usize) -> usize,
) -> Result<Vec<(String, Vec<f32>)>> {
    let mut found: Vec<(String, Vec<f32>)> = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::new();
    let mut tries = 0;
    while found.len() < count && tries < count * 20 && !ids.is_empty() {
        tries += 1;
        let id = &ids[pick(ids.len())];
        let Some(chunk) = Vectors::new(store).chunks_for(id)?.into_iter().next() else {
            continue;
        };
        let content = testing::memory_text(store, id, "content")?.unwrap_or_default();
        let words: Vec<&str> = content
            .split_whitespace()
            .filter(|w| w.chars().all(char::is_alphanumeric))
            .take(QUERY_WORDS)
            .collect();
        let text = words.join(" ");
        if words.is_empty() || !seen.insert(text.clone()) {
            continue;
        }
        found.push((text, floats(&chunk.embedding)));
    }
    if found.len() < count {
        return Err(format!("only {} of {count} queries found in the store", found.len()).into());
    }
    Ok(found)
}

/// Little-endian f32 bytes as floats.
pub fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Floats as little-endian f32 bytes, as stored.
pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn text<'r>(row: &'r Map<String, Value>, column: &str) -> Result<&'r str> {
    row.get(column)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("stored row has no text {column}").into())
}

fn bytes(row: &Map<String, Value>) -> Result<Vec<u8>> {
    row.get("embedding")
        .and_then(Value::as_array)
        .ok_or("stored chunk has no embedding")?
        .iter()
        .map(|b| {
            b.as_u64()
                .and_then(|b| u8::try_from(b).ok())
                .ok_or_else(|| "embedding byte out of range".into())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remind_me_core::db::queries;
    use remind_me_core::db::sqlite::SqliteTables;
    use remind_me_core::models::MemoryAddInput;

    fn engine() -> Database {
        Database::open_backend(Box::new(EngineTables::open_temporary().unwrap()), None).unwrap()
    }

    fn add(store: &Store<'_>, content: &str, category: &str) -> String {
        let input = MemoryAddInput {
            content: content.into(),
            category: category.into(),
            ..Default::default()
        };
        let id = queries::add_memory(store, input).unwrap().id;
        Vectors::new(store)
            .put(&id, 0, &to_bytes(&[0.5, 0.5]))
            .unwrap();
        id
    }

    /// A loaded copy passes parity; a row changed afterwards fails it.
    #[test]
    fn parity_catches_a_row_the_load_did_not_carry() {
        let _serial = super::super::serial();
        let root = std::env::temp_dir().join(format!("d1_parity_{}", std::process::id()));
        let bodies = Bodies::new(&root);
        let (source, target) = (engine(), {
            let tables = SqliteTables::open_in_memory().unwrap();
            Database::open_backend(Box::new(tables), None).unwrap()
        });
        let (s, t) = (source.store(), target.store());
        add(&s, "a fact worth keeping", "fact");
        let dialog = add(&s, &"a long dialog body ".repeat(100), DIALOG);
        copy_rows(&s, &t, &bodies).unwrap();
        assert_eq!(parity(&s, &t, &bodies).unwrap()["matched"], true);
        let stored = testing::memory_text(&t, &dialog, "content")
            .unwrap()
            .unwrap();
        assert!(
            stored.len() <= super::super::bodies::HEAD_BYTES,
            "the dialog keeps its head only"
        );
        let fact = testing::memory_ids(&t)
            .unwrap()
            .into_iter()
            .find(|id| *id != dialog)
            .unwrap();
        testing::set_memory_column(&t, &fact, "content", "changed").unwrap();
        assert!(parity(&s, &t, &bodies).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two memories sharing their leading words give one query, not two
    /// texts overwriting one vector.
    #[test]
    fn a_shared_query_text_is_used_once() {
        let _serial = super::super::serial();
        let db = engine();
        let store = db.store();
        let ids = [
            add(&store, "same words here", "fact"),
            add(&store, "same words here", "fact"),
            add(&store, "other words", "fact"),
        ];
        let mut next = 0;
        let pairs = queries(&store, &ids, 2, |len| {
            next += 1;
            (next - 1) % len
        })
        .unwrap();
        let texts: Vec<&str> = pairs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["same words here", "other words"]);
        assert!(
            queries(&store, &ids, 3, |len| len - 1).is_err(),
            "only two distinct"
        );
    }
}
