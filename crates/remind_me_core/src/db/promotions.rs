//! Storage for promotion: the `promotions` provenance table, and the reads
//! of `memories` and the entity graph that find what is ready to move up a
//! rung, on the engine's memories core (`db::engine::promotions`).
//!
//! Every read and write [`crate::promotion`] makes goes through here. The
//! rules stay there: which rung reads which category, the fact threshold,
//! the persona vitality floor, what makes a source unusable, and that
//! demotion is a read-time judgement.

use super::engine::{self, EngineLock};
use super::{Result, Store};

/// A memory's id and content.
#[derive(Debug, Clone, PartialEq)]
pub struct IdContent {
    pub id: String,
    pub content: String,
}

/// Facts that mention one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityFactGroup {
    pub entity_name: String,
    pub memory_ids: Vec<String>,
}

/// A scenario and its vitality.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    pub id: String,
    pub content: String,
    pub vitality: f64,
}

/// A persona statement as stored, before its sources are counted.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementRow {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub created_at: String,
}

/// The promotion table and reads, on the engine's memories core.
pub struct Promotions<'c> {
    core: &'c EngineLock,
}

impl<'c> Promotions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    // --- candidates ------------------------------------------------------

    /// Captured dialogs nothing has been decomposed from, newest first, at
    /// most `limit`.
    pub fn undecomposed_dialogs(&self, limit: usize) -> Result<Vec<IdContent>> {
        engine::promotions::undecomposed_dialogs(&self.core.lock(), limit)
    }

    /// How many captured dialogs nothing has been decomposed from.
    pub fn count_undecomposed_dialogs(&self) -> Result<usize> {
        engine::promotions::count_undecomposed_dialogs(&self.core.lock())
    }

    /// Groups of at least `min_facts` live `category` memories sharing an
    /// entity, none promoted at `rung`, largest first (ties by entity id),
    /// at most `limit`, each group's ids sorted.
    pub fn entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        limit: usize,
    ) -> Result<Vec<EntityFactGroup>> {
        engine::promotions::entity_fact_groups(&self.core.lock(), category, rung, min_facts, limit)
    }

    /// How many groups [`Promotions::entity_fact_groups`] would find
    /// without a limit.
    pub fn count_entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
    ) -> Result<usize> {
        engine::promotions::count_entity_fact_groups(&self.core.lock(), category, rung, min_facts)
    }

    /// Live, non-sensitive `category` memories at or above `floor`
    /// vitality, not promoted at `rung`, most vital first, at most `limit`.
    pub fn ready_scenarios(
        &self,
        category: &str,
        floor: f64,
        rung: &str,
        limit: usize,
    ) -> Result<Vec<ScoredMemory>> {
        engine::promotions::ready_scenarios(&self.core.lock(), category, floor, rung, limit)
    }

    /// How many memories [`Promotions::ready_scenarios`] would find without
    /// a limit.
    pub fn count_ready_scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<usize> {
        engine::promotions::count_ready_scenarios(&self.core.lock(), category, floor, rung)
    }

    // --- promoting -------------------------------------------------------

    /// Whether the live, unsuperseded memory `id` is sensitive, or `None`
    /// when `id` is not such a memory.
    pub fn live_source_sensitivity(&self, id: &str) -> Result<Option<bool>> {
        engine::promotions::live_source_sensitivity(&self.core.lock(), id)
    }

    /// The memories promoted at `rung` from a set including `source_id`.
    pub fn promoted_from(&self, source_id: &str, rung: &str) -> Result<Vec<String>> {
        engine::promotions::promoted_from(&self.core.lock(), source_id, rung)
    }

    /// The sources `promoted_id` was promoted from at `rung`.
    pub fn sources_at(&self, promoted_id: &str, rung: &str) -> Result<Vec<String>> {
        engine::promotions::sources_at(&self.core.lock(), promoted_id, rung)
    }

    /// Whether `id` is a live, unsuperseded memory.
    pub fn is_live(&self, id: &str) -> Result<bool> {
        engine::promotions::is_live(&self.core.lock(), id)
    }

    /// Record that `promoted_id` was promoted from `source_id` at `rung`.
    /// A pair already recorded is left as it is.
    pub fn record(
        &self,
        promoted_id: &str,
        source_id: &str,
        rung: &str,
        promoted_at: &str,
    ) -> Result<()> {
        engine::promotions::record(&mut self.core.lock(), promoted_id, source_id, rung, promoted_at)
    }

    // --- provenance ------------------------------------------------------

    /// Every source `memory_id` was promoted from, at any rung.
    pub fn sources_of(&self, memory_id: &str) -> Result<Vec<String>> {
        engine::promotions::sources_of(&self.core.lock(), memory_id)
    }

    /// Every memory promoted from `memory_id`, at any rung.
    pub fn derived_from(&self, memory_id: &str) -> Result<Vec<String>> {
        engine::promotions::derived_from(&self.core.lock(), memory_id)
    }

    /// How many of `promoted_id`'s sources are live and unsuperseded.
    pub fn surviving_sources(&self, promoted_id: &str) -> Result<usize> {
        engine::promotions::surviving_sources(&self.core.lock(), promoted_id)
    }

    /// Live, unsuperseded, non-sensitive `category` memories, most vital
    /// first.
    pub fn statements_by_vitality(&self, category: &str) -> Result<Vec<StatementRow>> {
        engine::promotions::statements(&self.core.lock(), category, true)
    }

    /// Live, unsuperseded `category` memories, sensitive or not, newest
    /// first.
    pub fn statements_newest_first(&self, category: &str) -> Result<Vec<StatementRow>> {
        engine::promotions::statements(&self.core.lock(), category, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn surviving_sources_counts_only_live_unsuperseded_sources() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories.insert(&NewMemory::new("live", "x", NOW)).unwrap();
        memories
            .insert(&NewMemory {
                superseded_by: Some("other".to_string()),
                ..NewMemory::new("old", "x", NOW)
            })
            .unwrap();
        let promotions = Promotions::new(&store);
        for source in ["live", "old", "missing"] {
            promotions.record("p", source, "rung", NOW).unwrap();
        }
        promotions.record("p", "live", "rung", NOW).unwrap();

        assert_eq!(promotions.surviving_sources("p").unwrap(), 1);
        assert_eq!(promotions.sources_of("p").unwrap().len(), 3);
        assert_eq!(promotions.derived_from("live").unwrap(), vec!["p"]);
    }

    #[test]
    fn live_source_sensitivity_is_none_for_a_superseded_source() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories
            .insert(&NewMemory {
                sensitive: true,
                ..NewMemory::new("s", "x", NOW)
            })
            .unwrap();
        memories
            .insert(&NewMemory {
                superseded_by: Some("s".to_string()),
                ..NewMemory::new("old", "x", NOW)
            })
            .unwrap();
        let promotions = Promotions::new(&store);
        assert_eq!(promotions.live_source_sensitivity("s").unwrap(), Some(true));
        assert_eq!(promotions.live_source_sensitivity("old").unwrap(), None);
        assert!(!promotions.is_live("old").unwrap());
    }
}
