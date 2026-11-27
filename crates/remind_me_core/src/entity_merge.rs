//! Person-confirmed entity merge and its revert (card #567, pack D101/D103).
//!
//! Merging B into A moves B's memory links and relations to A, folds B's
//! name and aliases into A's aliases, writes a merge-record memory that holds
//! everything needed to undo it, and deletes B, all in one store
//! transaction. Nothing here runs on its own: a caller names both sides and
//! confirms. [`preview`] changes nothing.
//!
//! The record is a fact memory with the triple `B entity.merged_into A` and
//! `metadata.entity_merge`. [`revert`] restores B from it exactly and writes
//! a compensating `entity.merge_reverted` fact that supersedes the record, as
//! a project revert does (#702).
//!
//! Relations: a B relation that would point A at itself is dropped; one whose
//! re-pointed triple A already has is dropped and A's row is left as it was,
//! so a revert never has to rebuild A's own rows.

use crate::db::derived::Origin;
use crate::db::entities::{Entities, RelationRow, StoredRelation};
use crate::db::memories::Memories;
use crate::db::{Result, Store, StoreError};
use crate::entity::{
    dedup_preserving_order, entity_id, entity_relation_id, get_entity_by_id, normalize_entity_name,
    resolve_entity, Entity,
};
use crate::models::Memory;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The predicate of a merge record.
pub const MERGED_INTO: &str = "entity.merged_into";
/// The predicate of the fact that reverts a merge record.
pub const MERGE_REVERTED: &str = "entity.merge_reverted";
/// The tag both records carry.
pub const MERGE_TAG: &str = "entity-merge";
/// Where a record keeps its undo data.
pub const META_KEY: &str = "entity_merge";
const SOURCE: &str = "entity-merge";

/// One side of a merge, as a preview shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Side {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub links: usize,
    pub relations: usize,
}

/// What a merge would do (or did).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub from: Side,
    pub into: Side,
    /// Memories linked to B that A does not have yet.
    pub links_moved: usize,
    /// Memories linked to both: B's link is dropped.
    pub links_shared: usize,
    pub relations_moved: usize,
    /// Self-loops and triples A already has.
    pub relations_dropped: usize,
    pub aliases_gained: Vec<String>,
    /// B's kind, when A has none.
    pub kind_filled: Option<String>,
}

/// The answer to a merge call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "merge", rename_all = "snake_case")]
pub enum Outcome {
    /// Nothing written: what `confirm` would do.
    Preview { plan: Plan },
    /// Applied; `record_id` undoes it with [`revert`].
    Merged { plan: Plan, record_id: String },
    /// B was already merged into A by `record_id`; nothing written.
    AlreadyMerged { record_id: String, into: String },
}

/// The answer to a revert call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "revert", rename_all = "snake_case")]
pub enum RevertOutcome {
    Reverted {
        restored: String,
        record_id: String,
        revert_id: String,
        links_restored: usize,
        relations_restored: usize,
    },
    /// The record was already reverted by `revert_id`; nothing written.
    AlreadyReverted {
        record_id: String,
        revert_id: String,
    },
}

/// One of B's mention links, as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LinkSnapshot {
    memory_id: String,
    created_at: String,
    /// A had the same memory linked already.
    shared: bool,
}

/// One of B's relations, as it was, and what the merge made of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RelationSnapshot {
    id: String,
    subject_entity_id: String,
    relation: String,
    object_entity_id: String,
    created_at: String,
    updated_at: String,
    /// The A-side row the merge wrote; `None` when the relation was dropped.
    moved_to: Option<String>,
}

/// Everything a revert needs, kept in the record's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    from: Entity,
    into_id: String,
    links: Vec<LinkSnapshot>,
    relations: Vec<RelationSnapshot>,
    aliases_gained: Vec<String>,
    kind_filled: bool,
    into_created_at_before: String,
}

/// What [`plan`] works out before anything is written.
struct Draft {
    plan: Plan,
    record: Record,
}

/// Preview merging `from` into `into`: counts only, nothing written.
pub fn preview(store: &Store<'_>, from: &str, into: &str) -> Result<Outcome> {
    match sides(store, from, into)? {
        Sides::Merged { record_id, into } => Ok(Outcome::AlreadyMerged { record_id, into }),
        Sides::Pair(pair) => Ok(Outcome::Preview {
            plan: draft(store, &pair.0, &pair.1)?.plan,
        }),
    }
}

/// Merge `from` into `into` in one transaction. A repeat after success is a
/// no-op naming the earlier merge.
pub fn merge(store: &Store<'_>, from: &str, into: &str) -> Result<Outcome> {
    store.transaction(|page| {
        let (b, a) = match sides(page, from, into)? {
            Sides::Merged { record_id, into } => {
                return Ok(Outcome::AlreadyMerged { record_id, into })
            }
            Sides::Pair(pair) => *pair,
        };
        let Draft { plan, record } = draft(page, &b, &a)?;
        apply(page, &b, &a, &plan, &record)?;
        let record_id = write_record(page, &b, &a, &plan, &record)?;
        Ok(Outcome::Merged { plan, record_id })
    })
}

/// Undo the merge `record_id` records. `from` must name the entity it
/// absorbed, so a stray id cannot restore the wrong thing.
pub fn revert(store: &Store<'_>, record_id: &str, from: &str) -> Result<RevertOutcome> {
    store.transaction(|page| revert_in(page, record_id, from))
}

enum Sides {
    Pair(Box<(Entity, Entity)>),
    Merged { record_id: String, into: String },
}

/// B by its own name or id only (an alias would find A after a merge); A
/// by name, id or alias. Refuses what may not be merged.
fn sides(store: &Store<'_>, from: &str, into: &str) -> Result<Sides> {
    let Some(b) = exact(store, from)? else {
        return match live_record_for(store, from)? {
            Some(record) => Ok(Sides::Merged {
                record_id: record.id,
                into: record.object.unwrap_or_default(),
            }),
            None => Err(StoreError::Invalid(format!("no entity named {from:?}"))),
        };
    };
    let a = match exact(store, into)? {
        Some(a) => a,
        None => resolve_entity(store, into)?
            .ok_or_else(|| StoreError::Invalid(format!("no entity named {into:?}")))?,
    };
    refuse(store, &b, &a)?;
    Ok(Sides::Pair(Box::new((b, a))))
}

fn exact(store: &Store<'_>, name_or_id: &str) -> Result<Option<Entity>> {
    let trimmed = name_or_id.trim();
    if trimmed.is_empty() {
        return Err(StoreError::Invalid("entity name is empty".into()));
    }
    match get_entity_by_id(store, trimmed)? {
        Some(e) => Ok(Some(e)),
        None => get_entity_by_id(store, &entity_id(trimmed)),
    }
}

fn refuse(store: &Store<'_>, b: &Entity, a: &Entity) -> Result<()> {
    if b.id == a.id {
        return Err(StoreError::Invalid(format!(
            "{:?} and {:?} are the same entity",
            b.name, a.name
        )));
    }
    for side in [b, a] {
        if crate::topic::is_topic_name(&side.name) {
            return Err(StoreError::Invalid(format!(
                "{:?} is a topic; topics are curated with the topic tools",
                side.name
            )));
        }
    }
    let is_project =
        b.kind.as_deref() == Some("project") || crate::project::has_claims(store, &b.name)?;
    if is_project {
        return Err(StoreError::Invalid(format!(
            "{:?} is a project and its claims are keyed on its name; merge the other entity into it instead",
            b.name
        )));
    }
    Ok(())
}

/// The newest live merge record whose absorbed name is `name`.
fn live_record_for(store: &Store<'_>, name: &str) -> Result<Option<Memory>> {
    let key = normalize_entity_name(name);
    let found = Memories::new(store)
        .with_predicate_prefix(MERGED_INTO)?
        .into_iter()
        .filter(|m| m.superseded_by.is_none())
        .filter(|m| m.subject.as_deref().map(normalize_entity_name).as_deref() == Some(&key))
        .max_by(|x, y| x.created_at.cmp(&y.created_at));
    Ok(found)
}

fn relations_of(store: &Store<'_>, id: &str) -> Result<Vec<StoredRelation>> {
    Ok(Entities::new(store)
        .relations_oldest_first()?
        .into_iter()
        .filter(|r| r.subject_entity_id == id || r.object_entity_id == id)
        .collect())
}

fn draft(store: &Store<'_>, b: &Entity, a: &Entity) -> Result<Draft> {
    let entities = Entities::new(store);
    let a_links = Memories::new(store).linked_ids(&a.id)?;
    let links: Vec<LinkSnapshot> = entities
        .links_oldest_first()?
        .into_iter()
        .filter(|(_, entity, _)| *entity == b.id)
        .map(|(memory_id, _, created_at)| LinkSnapshot {
            shared: a_links.contains(&memory_id),
            memory_id,
            created_at,
        })
        .collect();

    let b_relations = relations_of(store, &b.id)?;
    let a_relations = relations_of(store, &a.id)?;
    let mut taken: HashSet<String> = a_relations.iter().map(|r| r.id.clone()).collect();
    let relations: Vec<RelationSnapshot> = b_relations
        .into_iter()
        .map(|r| {
            let moved_to =
                moved_relation_id(&r, &b.id, &a.id).filter(|id| taken.insert(id.clone()));
            RelationSnapshot {
                id: r.id,
                subject_entity_id: r.subject_entity_id,
                relation: r.relation,
                object_entity_id: r.object_entity_id,
                created_at: r.created_at,
                updated_at: r.updated_at,
                moved_to,
            }
        })
        .collect();

    let merged_aliases = dedup_preserving_order(
        a.aliases
            .iter()
            .cloned()
            .chain(b.aliases.iter().cloned())
            .chain(std::iter::once(b.name.clone()))
            .filter(|alias| normalize_entity_name(alias) != normalize_entity_name(&a.name)),
    );
    let aliases_gained: Vec<String> = merged_aliases
        .iter()
        .filter(|alias| !a.aliases.contains(alias))
        .cloned()
        .collect();
    let kind_filled = a.kind.is_none().then(|| b.kind.clone()).flatten();

    let links_shared = links.iter().filter(|l| l.shared).count();
    let relations_moved = relations.iter().filter(|r| r.moved_to.is_some()).count();
    let plan = Plan {
        from: side(b, links.len(), relations.len()),
        into: side(a, a_links.len(), a_relations.len()),
        links_moved: links.len() - links_shared,
        links_shared,
        relations_moved,
        relations_dropped: relations.len() - relations_moved,
        aliases_gained: aliases_gained.clone(),
        kind_filled: kind_filled.clone(),
    };
    let record = Record {
        from: b.clone(),
        into_id: a.id.clone(),
        links,
        relations,
        aliases_gained,
        kind_filled: kind_filled.is_some(),
        into_created_at_before: a.created_at.clone(),
    };
    Ok(Draft { plan, record })
}

fn side(e: &Entity, links: usize, relations: usize) -> Side {
    Side {
        id: e.id.clone(),
        name: e.name.clone(),
        kind: e.kind.clone(),
        aliases: e.aliases.clone(),
        links,
        relations,
    }
}

/// A relation end, with B replaced by A.
fn swap_end<'x>(end: &'x str, b: &str, a: &'x str) -> &'x str {
    if end == b {
        a
    } else {
        end
    }
}

/// The id `r` gets once B's end points at A; `None` for a self-loop.
fn moved_relation_id(r: &StoredRelation, b: &str, a: &str) -> Option<String> {
    let subject = swap_end(&r.subject_entity_id, b, a);
    let object = swap_end(&r.object_entity_id, b, a);
    (subject != object).then(|| entity_relation_id(subject, &r.relation, object))
}

fn apply(store: &Store<'_>, b: &Entity, a: &Entity, plan: &Plan, record: &Record) -> Result<()> {
    let entities = Entities::new(store);
    for r in &record.relations {
        entities.delete_relation(&r.id)?;
        let Some(moved_id) = &r.moved_to else {
            continue;
        };
        entities.insert_relation_or_ignore(
            &RelationRow {
                id: moved_id,
                subject_entity_id: swap_end(&r.subject_entity_id, &b.id, &a.id),
                relation: &r.relation,
                object_entity_id: swap_end(&r.object_entity_id, &b.id, &a.id),
                created_at: &r.created_at,
                updated_at: &r.updated_at,
                node_id: None,
            },
            Origin::Local,
        )?;
    }
    // Links: repoint keeps each link's created_at and drops one A has.
    entities.repoint(&b.id, &a.id)?;

    let aliases: Vec<String> = a
        .aliases
        .iter()
        .chain(plan.aliases_gained.iter())
        .cloned()
        .collect();
    let kind = a.kind.clone().or_else(|| plan.kind_filled.clone());
    let created_at = a.created_at.clone().min(b.created_at.clone());
    entities.set_merged(&a.id, kind.as_deref(), &aliases, &created_at)?;
    entities.delete(&b.id)
}

fn write_record(
    store: &Store<'_>,
    b: &Entity,
    a: &Entity,
    plan: &Plan,
    record: &Record,
) -> Result<String> {
    let content = format!(
        "Entity {:?} merged into {:?}: {} links and {} relations moved, {} relations dropped.",
        b.name, a.name, plan.links_moved, plan.relations_moved, plan.relations_dropped
    );
    let metadata = serde_json::json!({ META_KEY: record });
    let mut row =
        crate::project::fact_from(SOURCE, &content, vec![MERGE_TAG.to_string()], metadata);
    row.subject = Some(b.name.clone());
    row.predicate = Some(MERGED_INTO.to_string());
    row.object = Some(a.name.clone());
    Memories::new(store).insert(&row)?;
    Ok(row.id)
}

fn revert_in(store: &Store<'_>, record_id: &str, from: &str) -> Result<RevertOutcome> {
    let memories = Memories::new(store);
    let row = memories
        .get_live(record_id)?
        .filter(|m| m.predicate.as_deref() == Some(MERGED_INTO))
        .ok_or_else(|| StoreError::Invalid(format!("{record_id} is not an entity merge record")))?;
    if let Some(revert_id) = row.superseded_by.clone() {
        return Ok(RevertOutcome::AlreadyReverted {
            record_id: record_id.to_string(),
            revert_id,
        });
    }
    let record: Record = row
        .metadata
        .get(META_KEY)
        .cloned()
        .ok_or_else(|| StoreError::Invalid(format!("{record_id} has no {META_KEY} metadata")))
        .and_then(|v| {
            serde_json::from_value(v).map_err(|e| {
                StoreError::Invalid(format!("{record_id}: unreadable merge record: {e}"))
            })
        })?;
    if normalize_entity_name(from) != normalize_entity_name(&record.from.name)
        && from.trim() != record.from.id
    {
        return Err(StoreError::Invalid(format!(
            "{record_id} merged {:?}, not {from:?}",
            record.from.name
        )));
    }
    let entities = Entities::new(store);
    if entities.exists(&record.from.id)? {
        return Err(StoreError::Invalid(format!(
            "an entity {:?} exists again; merge or delete it before reverting",
            record.from.name
        )));
    }
    let a = entities.get(&record.into_id)?.ok_or_else(|| {
        StoreError::Invalid(format!("{record_id}: the entity it merged into is gone"))
    })?;

    let b = &record.from;
    entities.insert(b, None)?;
    let links_restored = restore_links(store, &record, &a.id)?;
    let relations_restored = restore_relations(store, &record)?;
    restore_into(store, &record, &a)?;

    let content = format!("Entity merge of {:?} into {:?} reverted.", b.name, a.name);
    let metadata = serde_json::json!({ META_KEY: { "reverts": record_id } });
    let mut revert_row =
        crate::project::fact_from(SOURCE, &content, vec![MERGE_TAG.to_string()], metadata);
    revert_row.subject = Some(b.name.clone());
    revert_row.predicate = Some(MERGE_REVERTED.to_string());
    revert_row.object = Some(a.name.clone());
    memories.insert(&revert_row)?;
    memories.set_superseded_by(record_id, &revert_row.id, Some(&revert_row.created_at))?;

    Ok(RevertOutcome::Reverted {
        restored: b.name.clone(),
        record_id: record_id.to_string(),
        revert_id: revert_row.id,
        links_restored,
        relations_restored,
    })
}

/// Move B's own links back; shared ones are re-added to B and stay on A.
/// A memory deleted since the merge is skipped.
fn restore_links(store: &Store<'_>, record: &Record, a_id: &str) -> Result<usize> {
    let entities = Entities::new(store);
    let memories = Memories::new(store);
    let mut restored = 0;
    for link in &record.links {
        if !memories.exists(&link.memory_id)? {
            continue;
        }
        if !link.shared {
            entities.unlink(&link.memory_id, a_id)?;
        }
        entities.link(
            &link.memory_id,
            &record.from.id,
            &link.created_at,
            Origin::Local,
        )?;
        restored += 1;
    }
    Ok(restored)
}

/// Delete the A-side rows the merge wrote and put B's rows back.
fn restore_relations(store: &Store<'_>, record: &Record) -> Result<usize> {
    let entities = Entities::new(store);
    for r in &record.relations {
        if let Some(moved_id) = &r.moved_to {
            entities.delete_relation(moved_id)?;
        }
        entities.insert_relation_or_ignore(
            &RelationRow {
                id: &r.id,
                subject_entity_id: &r.subject_entity_id,
                relation: &r.relation,
                object_entity_id: &r.object_entity_id,
                created_at: &r.created_at,
                updated_at: &r.updated_at,
                node_id: None,
            },
            Origin::Local,
        )?;
    }
    Ok(record.relations.len())
}

/// Take back what A gained, leaving anything A got since.
fn restore_into(store: &Store<'_>, record: &Record, a: &Entity) -> Result<()> {
    let aliases: Vec<String> = a
        .aliases
        .iter()
        .filter(|alias| !record.aliases_gained.contains(alias))
        .cloned()
        .collect();
    let kind = if record.kind_filled && a.kind == record.from.kind {
        None
    } else {
        a.kind.clone()
    };
    let created_at = if a.created_at == record.from.created_at {
        record.into_created_at_before.clone()
    } else {
        a.created_at.clone()
    };
    Entities::new(store).set_merged(&a.id, kind.as_deref(), &aliases, &created_at)
}
