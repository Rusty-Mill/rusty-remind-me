//! Promotion provenance and the promotion reads on the engine core
//! (ADR-0023, core PR 3c). [`crate::db::promotions`] calls
//! these when its store carries the core.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::{core_ref, pair_engine_id, EngineTables};
use crate::db::promotions::{EntityFactGroup, IdContent, ScoredMemory, StatementRow};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// Index marker: a promotion by its source.
pub struct BySource;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type PromotionTable = GenericMmapStore<PromotionRecord, BySource, Slot>;

/// One `promotions` row, keyed by its (promoted, source) pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionRecord {
    engine_id: Uuid,
    promoted_id: String,
    source_id: String,
    rung: String,
    promoted_at: String,
    slot: i64,
}

impl Record for PromotionRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for PromotionRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::PromotionRecord@1";
}

impl IndexedField<BySource> for PromotionRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.source_id
    }
}

impl ScannableField<Slot> for PromotionRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

fn promotions(core: &CoreTables) -> Vec<PromotionRecord> {
    core.promotions
        .all_ids()
        .into_iter()
        .filter_map(|id| core.promotions.get(id))
        .collect()
}

fn live(row: &MemoryRow) -> bool {
    row.deleted_at.is_none() && row.superseded_by.is_none()
}

/// A decision that was reverted or abandoned: no longer grounds for
/// anything built on it.
fn discredited(row: &MemoryRow) -> bool {
    row.memory_type == "decision" && matches!(row.outcome.as_deref(), Some("reverted" | "abandoned"))
}

/// Ids of promoted memories with at least one discredited source.
fn built_on_discredited(core: &CoreTables) -> HashSet<String> {
    promotions(core)
        .into_iter()
        .filter(|p| memories::row(core, &p.source_id).is_some_and(|row| discredited(&row)))
        .map(|p| p.promoted_id)
        .collect()
}

fn promoted_at_rung(core: &CoreTables, rung: &str) -> HashSet<String> {
    promotions(core)
        .into_iter()
        .filter(|p| p.rung == rung)
        .map(|p| p.source_id)
        .collect()
}

/// Record a promotion unless the pair is already recorded: `INSERT OR
/// IGNORE`.
pub(crate) fn record(
    tables: &mut EngineTables,
    promoted_id: &str,
    source_id: &str,
    rung: &str,
    promoted_at: &str,
) -> Result<()> {
    let engine_id = pair_engine_id(promoted_id, source_id);
    if core_ref(tables)?.promotions.get(engine_id).is_some() {
        return Ok(());
    }
    let record = PromotionRecord {
        engine_id,
        promoted_id: promoted_id.to_string(),
        source_id: source_id.to_string(),
        rung: rung.to_string(),
        promoted_at: promoted_at.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::Promotion(engine_id, Some(record))])
}

/// The field `pick` of every promotion `keep` selects, sorted and without
/// repeats when `distinct`.
fn column(
    tables: &EngineTables,
    keep: impl Fn(&PromotionRecord) -> bool,
    pick: impl Fn(PromotionRecord) -> String,
    distinct: bool,
) -> Result<Vec<String>> {
    let mut values: Vec<String> = promotions(core_ref(tables)?)
        .into_iter()
        .filter(|p| keep(p))
        .map(pick)
        .collect();
    values.sort();
    if distinct {
        values.dedup();
    }
    Ok(values)
}

pub(crate) fn promoted_from(
    tables: &EngineTables,
    source_id: &str,
    rung: &str,
) -> Result<Vec<String>> {
    column(
        tables,
        |p| p.source_id == source_id && p.rung == rung,
        |p| p.promoted_id,
        true,
    )
}

pub(crate) fn sources_at(
    tables: &EngineTables,
    promoted_id: &str,
    rung: &str,
) -> Result<Vec<String>> {
    column(
        tables,
        |p| p.promoted_id == promoted_id && p.rung == rung,
        |p| p.source_id,
        false,
    )
}

pub(crate) fn sources_of(tables: &EngineTables, memory_id: &str) -> Result<Vec<String>> {
    column(
        tables,
        |p| p.promoted_id == memory_id,
        |p| p.source_id,
        false,
    )
}

pub(crate) fn derived_from(tables: &EngineTables, memory_id: &str) -> Result<Vec<String>> {
    column(
        tables,
        |p| p.source_id == memory_id,
        |p| p.promoted_id,
        false,
    )
}

/// How many of `promoted_id`'s sources are live and unsuperseded.
pub(crate) fn surviving_sources(tables: &EngineTables, promoted_id: &str) -> Result<usize> {
    let core = core_ref(tables)?;
    Ok(promotions(core)
        .into_iter()
        .filter(|p| p.promoted_id == promoted_id)
        .filter(|p| {
            memories::row(core, &p.source_id).is_some_and(|row| live(&row) && !discredited(&row))
        })
        .count())
}

pub(crate) fn live_source_sensitivity(tables: &EngineTables, id: &str) -> Result<Option<bool>> {
    Ok(memories::row(core_ref(tables)?, id)
        .filter(live)
        .map(|row| row.sensitive))
}

pub(crate) fn is_live(tables: &EngineTables, id: &str) -> Result<bool> {
    Ok(memories::row(core_ref(tables)?, id).is_some_and(|row| live(&row)))
}

/// Captured dialogs nothing names as `source_capture_id`, newest first, ties
/// by id descending.
fn undecomposed_dialog_rows(core: &CoreTables) -> Vec<MemoryRow> {
    let named: HashSet<String> = memories::rows(core)
        .filter_map(|row| row.source_capture_id)
        .collect();
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|row| row.deleted_at.is_none() && row.category == "dialog")
        .filter(|row| row.source_capture_id.is_none())
        .filter(|row| row.capture_id.as_ref().is_some_and(|c| !named.contains(c)))
        .collect();
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
    rows
}

pub(crate) fn undecomposed_dialogs(tables: &EngineTables, limit: usize) -> Result<Vec<IdContent>> {
    Ok(undecomposed_dialog_rows(core_ref(tables)?)
        .into_iter()
        .take(limit)
        .map(|row| IdContent {
            id: row.id,
            content: row.content,
        })
        .collect())
}

pub(crate) fn count_undecomposed_dialogs(tables: &EngineTables) -> Result<usize> {
    Ok(undecomposed_dialog_rows(core_ref(tables)?).len())
}

/// Live `category` memories not promoted at `rung`, grouped by an entity
/// they mention, groups of at least `min_facts`: largest first, ties by
/// entity id, ids sorted within a group.
fn fact_groups(
    core: &CoreTables,
    category: &str,
    rung: &str,
    min_facts: usize,
) -> Vec<EntityFactGroup> {
    let promoted = promoted_at_rung(core, rung);
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (memory_id, entity_id) in super::graph::link_pairs(core) {
        let eligible = memories::row(core, &memory_id)
            .is_some_and(|row| live(&row) && row.category == category)
            && !promoted.contains(&memory_id);
        if eligible && super::graph::entity_name(core, &entity_id).is_some() {
            groups.entry(entity_id).or_default().push(memory_id);
        }
    }
    let mut found: Vec<(String, Vec<String>)> = groups
        .into_iter()
        .filter(|(_, ids)| ids.len() >= min_facts)
        .collect();
    found.sort_by(|(ea, a), (eb, b)| b.len().cmp(&a.len()).then_with(|| ea.cmp(eb)));
    found
        .into_iter()
        .filter_map(|(entity_id, mut memory_ids)| {
            memory_ids.sort();
            Some(EntityFactGroup {
                entity_name: super::graph::entity_name(core, &entity_id)?,
                memory_ids,
            })
        })
        .collect()
}

pub(crate) fn entity_fact_groups(
    tables: &EngineTables,
    category: &str,
    rung: &str,
    min_facts: usize,
    limit: usize,
) -> Result<Vec<EntityFactGroup>> {
    let mut groups = fact_groups(core_ref(tables)?, category, rung, min_facts);
    groups.truncate(limit);
    Ok(groups)
}

pub(crate) fn count_entity_fact_groups(
    tables: &EngineTables,
    category: &str,
    rung: &str,
    min_facts: usize,
) -> Result<usize> {
    Ok(fact_groups(core_ref(tables)?, category, rung, min_facts).len())
}

/// Live, non-sensitive `category` memories at or above `floor` stored
/// vitality, not promoted at `rung`: most vital first, ties by id.
fn scenarios(core: &CoreTables, category: &str, floor: f64, rung: &str) -> Vec<MemoryRow> {
    let promoted = promoted_at_rung(core, rung);
    // A scenario resting on a reverted or abandoned decision is not a
    // durable statement about the user; withheld, not deleted.
    let withheld = built_on_discredited(core);
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|row| live(row) && !row.sensitive && row.category == category)
        .filter(|row| row.vitality >= floor && !promoted.contains(&row.id))
        .filter(|row| !withheld.contains(&row.id))
        .collect();
    rows.sort_by(|a, b| {
        b.vitality
            .total_cmp(&a.vitality)
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

pub(crate) fn ready_scenarios(
    tables: &EngineTables,
    category: &str,
    floor: f64,
    rung: &str,
    limit: usize,
) -> Result<Vec<ScoredMemory>> {
    Ok(scenarios(core_ref(tables)?, category, floor, rung)
        .into_iter()
        .take(limit)
        .map(|row| ScoredMemory {
            id: row.id,
            content: row.content,
            vitality: row.vitality,
        })
        .collect())
}

pub(crate) fn count_ready_scenarios(
    tables: &EngineTables,
    category: &str,
    floor: f64,
    rung: &str,
) -> Result<usize> {
    Ok(scenarios(core_ref(tables)?, category, floor, rung).len())
}

/// Live `category` memories as statements, non-sensitive and most vital
/// first when `by_vitality`, else all of them newest first; ties by id.
pub(crate) fn statements(
    tables: &EngineTables,
    category: &str,
    by_vitality: bool,
) -> Result<Vec<StatementRow>> {
    let mut rows: Vec<MemoryRow> = memories::rows(core_ref(tables)?)
        .filter(|row| live(row) && row.category == category)
        .filter(|row| !by_vitality || !row.sensitive)
        .collect();
    if by_vitality {
        rows.sort_by(|a, b| {
            b.vitality
                .total_cmp(&a.vitality)
                .then_with(|| a.id.cmp(&b.id))
        });
    } else {
        rows.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    Ok(rows
        .into_iter()
        .map(|row| StatementRow {
            id: row.id,
            content: row.content,
            vitality: row.vitality,
            created_at: row.created_at,
        })
        .collect())
}
