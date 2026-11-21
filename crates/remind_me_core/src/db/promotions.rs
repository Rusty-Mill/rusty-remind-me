//! Storage for promotion: the `promotions` provenance table, and the reads
//! of `memories` and the entity graph that find what is ready to move up a
//! rung, behind the [`PromotionStore`] port (ADR-0035), which the engine's
//! memories core implements in `db::engine::promotions`.
//!
//! Every read and write [`crate::promotion`] makes goes through here. The
//! rules stay there: which rung reads which category, the fact threshold,
//! the persona vitality floor, what makes a source unusable, and that
//! demotion is a read-time judgement.

use super::backend::BackendLock;
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

/// The port for the `promotions` table and the promotion reads over the
/// memories core and the entity graph (ADR-0035). [`conformance`] holds the
/// behaviour every implementation must show.
pub trait PromotionStore {
    /// Captured dialogs nothing has been decomposed from, newest first, at
    /// most `limit`.
    fn undecomposed_dialogs(&self, limit: usize) -> Result<Vec<IdContent>>;

    /// How many captured dialogs nothing has been decomposed from.
    fn count_undecomposed_dialogs(&self) -> Result<usize>;

    /// Groups of at least `min_facts` live `category` memories sharing an
    /// entity, none promoted at `rung`, largest first (ties by entity id),
    /// at most `limit`, each group's ids sorted.
    fn entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
        limit: usize,
    ) -> Result<Vec<EntityFactGroup>>;

    /// How many groups [`PromotionStore::entity_fact_groups`] would find
    /// without a limit.
    fn count_entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
    ) -> Result<usize>;

    /// Live, non-sensitive `category` memories at or above `floor`
    /// vitality, not promoted at `rung`, most vital first, at most `limit`.
    fn ready_scenarios(
        &self,
        category: &str,
        floor: f64,
        rung: &str,
        limit: usize,
    ) -> Result<Vec<ScoredMemory>>;

    /// How many memories [`PromotionStore::ready_scenarios`] would find without
    /// a limit.
    fn count_ready_scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<usize>;

    /// Whether the live, unsuperseded memory `id` is sensitive, or `None`
    /// when `id` is not such a memory.
    fn live_source_sensitivity(&self, id: &str) -> Result<Option<bool>>;

    /// The memories promoted at `rung` from a set including `source_id`.
    fn promoted_from(&self, source_id: &str, rung: &str) -> Result<Vec<String>>;

    /// The sources `promoted_id` was promoted from at `rung`.
    fn sources_at(&self, promoted_id: &str, rung: &str) -> Result<Vec<String>>;

    /// Whether `id` is a live, unsuperseded memory.
    fn is_live(&self, id: &str) -> Result<bool>;

    /// Record that `promoted_id` was promoted from `source_id` at `rung`.
    /// A pair already recorded is left as it is.
    fn record(
        &mut self,
        promoted_id: &str,
        source_id: &str,
        rung: &str,
        promoted_at: &str,
    ) -> Result<()>;

    /// Every source `memory_id` was promoted from, at any rung.
    fn sources_of(&self, memory_id: &str) -> Result<Vec<String>>;

    /// Every memory promoted from `memory_id`, at any rung.
    fn derived_from(&self, memory_id: &str) -> Result<Vec<String>>;

    /// How many of `promoted_id`'s sources are live and unsuperseded.
    fn surviving_sources(&self, promoted_id: &str) -> Result<usize>;

    /// Live, unsuperseded, non-sensitive `category` memories, most vital
    /// first.
    fn statements_by_vitality(&self, category: &str) -> Result<Vec<StatementRow>>;

    /// Live, unsuperseded `category` memories, sensitive or not, newest
    /// first.
    fn statements_newest_first(&self, category: &str) -> Result<Vec<StatementRow>>;
}

/// The promotion table and reads, on the engine's memories core.
pub struct Promotions<'c> {
    backend: &'c BackendLock,
}

impl<'c> Promotions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`PromotionStore::undecomposed_dialogs`].
    pub fn undecomposed_dialogs(&self, limit: usize) -> Result<Vec<IdContent>> {
        PromotionStore::undecomposed_dialogs(&*self.backend.lock()?, limit)
    }

    /// See [`PromotionStore::count_undecomposed_dialogs`].
    pub fn count_undecomposed_dialogs(&self) -> Result<usize> {
        PromotionStore::count_undecomposed_dialogs(&*self.backend.lock()?)
    }

    /// See [`PromotionStore::entity_fact_groups`].
    pub fn entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
        limit: usize,
    ) -> Result<Vec<EntityFactGroup>> {
        PromotionStore::entity_fact_groups(
            &*self.backend.lock()?,
            category,
            rung,
            min_facts,
            min_sources,
            limit,
        )
    }

    /// See [`PromotionStore::count_entity_fact_groups`].
    pub fn count_entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
    ) -> Result<usize> {
        PromotionStore::count_entity_fact_groups(
            &*self.backend.lock()?,
            category,
            rung,
            min_facts,
            min_sources,
        )
    }

    /// See [`PromotionStore::ready_scenarios`].
    pub fn ready_scenarios(
        &self,
        category: &str,
        floor: f64,
        rung: &str,
        limit: usize,
    ) -> Result<Vec<ScoredMemory>> {
        PromotionStore::ready_scenarios(&*self.backend.lock()?, category, floor, rung, limit)
    }

    /// See [`PromotionStore::count_ready_scenarios`].
    pub fn count_ready_scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<usize> {
        PromotionStore::count_ready_scenarios(&*self.backend.lock()?, category, floor, rung)
    }

    /// See [`PromotionStore::live_source_sensitivity`].
    pub fn live_source_sensitivity(&self, id: &str) -> Result<Option<bool>> {
        PromotionStore::live_source_sensitivity(&*self.backend.lock()?, id)
    }

    /// See [`PromotionStore::promoted_from`].
    pub fn promoted_from(&self, source_id: &str, rung: &str) -> Result<Vec<String>> {
        PromotionStore::promoted_from(&*self.backend.lock()?, source_id, rung)
    }

    /// See [`PromotionStore::sources_at`].
    pub fn sources_at(&self, promoted_id: &str, rung: &str) -> Result<Vec<String>> {
        PromotionStore::sources_at(&*self.backend.lock()?, promoted_id, rung)
    }

    /// See [`PromotionStore::is_live`].
    pub fn is_live(&self, id: &str) -> Result<bool> {
        PromotionStore::is_live(&*self.backend.lock()?, id)
    }

    /// See [`PromotionStore::record`].
    pub fn record(
        &self,
        promoted_id: &str,
        source_id: &str,
        rung: &str,
        promoted_at: &str,
    ) -> Result<()> {
        PromotionStore::record(
            &mut *self.backend.lock()?,
            promoted_id,
            source_id,
            rung,
            promoted_at,
        )
    }

    /// See [`PromotionStore::sources_of`].
    pub fn sources_of(&self, memory_id: &str) -> Result<Vec<String>> {
        PromotionStore::sources_of(&*self.backend.lock()?, memory_id)
    }

    /// See [`PromotionStore::derived_from`].
    pub fn derived_from(&self, memory_id: &str) -> Result<Vec<String>> {
        PromotionStore::derived_from(&*self.backend.lock()?, memory_id)
    }

    /// See [`PromotionStore::surviving_sources`].
    pub fn surviving_sources(&self, promoted_id: &str) -> Result<usize> {
        PromotionStore::surviving_sources(&*self.backend.lock()?, promoted_id)
    }

    /// See [`PromotionStore::statements_by_vitality`].
    pub fn statements_by_vitality(&self, category: &str) -> Result<Vec<StatementRow>> {
        PromotionStore::statements_by_vitality(&*self.backend.lock()?, category)
    }

    /// See [`PromotionStore::statements_newest_first`].
    pub fn statements_newest_first(&self, category: &str) -> Result<Vec<StatementRow>> {
        PromotionStore::statements_newest_first(&*self.backend.lock()?, category)
    }
}

/// The behaviour every [`PromotionStore`] must show, run against each backend
/// by [`promotion_conformance`] (ADR-0035).
///
/// Promotion reads are over memories and their entity links, so a backend
/// under test implements [`MemoryStore`] and
/// [`EntityStore`](crate::db::entities::EntityStore) too, and the checks
/// seed through them.
#[cfg(test)]
pub(crate) mod conformance {
    use super::PromotionStore;
    use crate::db::curation::conformance::{seed_corpus, NOW};
    use crate::db::entities::EntityStore;
    use crate::db::memories::{MemoryStore, NewMemory};

    /// Every promotion read over the curation corpus, with promotions
    /// recorded across two rungs.
    pub(crate) fn promotion_reads_see_the_corpus_as_the_rules_say<
        S: PromotionStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        seed_corpus(store);
        for (p, src, rung) in [
            ("sc1", "f4", "scenario"),
            ("sc1", "f2", "scenario"),
            ("sc1", "f2", "scenario"),
            ("p1", "sc1", "persona"),
            ("p1", "gone", "persona"),
        ] {
            PromotionStore::record(store, p, src, rung, NOW).unwrap();
        }
        let dialogs: Vec<String> = PromotionStore::undecomposed_dialogs(store, 10)
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(dialogs, ["d1"]);
        assert_eq!(
            PromotionStore::count_undecomposed_dialogs(store,).unwrap(),
            1
        );
        let groups =
            PromotionStore::entity_fact_groups(store, "fact", "scenario", 2, 1, 10).unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].entity_name, "Rust");
        assert_eq!(
            groups[0].memory_ids,
            ["f1", "f3"],
            "f2 is superseded and f4 promoted at this rung, which also leaves Tokio one fact"
        );
        assert_eq!(
            PromotionStore::entity_fact_groups(store, "fact", "other", 1, 1, 1)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            PromotionStore::count_entity_fact_groups(store, "fact", "other", 1, 1).unwrap(),
            3
        );
        let ready: Vec<String> =
            PromotionStore::ready_scenarios(store, "scenario", 0.5, "persona", 10)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
        assert_eq!(ready, ["sc2"], "sc1 is promoted, sc3 too faint");
        assert_eq!(
            PromotionStore::count_ready_scenarios(store, "scenario", 0.5, "persona").unwrap(),
            1
        );
        assert_eq!(
            PromotionStore::live_source_sensitivity(store, "f3").unwrap(),
            Some(true)
        );
        assert_eq!(
            PromotionStore::live_source_sensitivity(store, "f2").unwrap(),
            None
        );
        assert_eq!(
            PromotionStore::promoted_from(store, "f2", "scenario").unwrap(),
            ["sc1"]
        );
        assert_eq!(
            PromotionStore::sources_at(store, "sc1", "scenario").unwrap(),
            ["f2", "f4"]
        );
        assert!(PromotionStore::is_live(store, "f1").unwrap());
        assert!(!PromotionStore::is_live(store, "f2").unwrap());
        assert_eq!(
            PromotionStore::sources_of(store, "p1").unwrap(),
            ["gone", "sc1"]
        );
        assert_eq!(PromotionStore::derived_from(store, "sc1").unwrap(), ["p1"]);
        assert_eq!(PromotionStore::surviving_sources(store, "sc1").unwrap(), 1);
        let by_vitality: Vec<String> = PromotionStore::statements_by_vitality(store, "fact")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            by_vitality,
            ["f1", "f4"],
            "non-sensitive, live, unsuperseded"
        );
        let newest: Vec<String> = PromotionStore::statements_newest_first(store, "fact")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(newest, ["f1", "f4", "f3"]);
    }

    pub(crate) fn surviving_sources_counts_only_live_unsuperseded_sources<
        S: PromotionStore + MemoryStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(store, &NewMemory::new("live", "x", NOW)).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                superseded_by: Some("other".to_string()),
                ..NewMemory::new("old", "x", NOW)
            },
        )
        .unwrap();
        for source in ["live", "old", "missing"] {
            PromotionStore::record(store, "p", source, "rung", NOW).unwrap();
        }
        PromotionStore::record(store, "p", "live", "rung", NOW).unwrap();

        assert_eq!(PromotionStore::surviving_sources(store, "p").unwrap(), 1);
        assert_eq!(PromotionStore::sources_of(store, "p").unwrap().len(), 3);
        assert_eq!(
            PromotionStore::derived_from(store, "live").unwrap(),
            vec!["p"]
        );
    }

    pub(crate) fn live_source_sensitivity_is_none_for_a_superseded_source<
        S: PromotionStore + MemoryStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(
            store,
            &NewMemory {
                sensitive: true,
                ..NewMemory::new("s", "x", NOW)
            },
        )
        .unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                superseded_by: Some("s".to_string()),
                ..NewMemory::new("old", "x", NOW)
            },
        )
        .unwrap();
        assert_eq!(
            PromotionStore::live_source_sensitivity(store, "s").unwrap(),
            Some(true)
        );
        assert_eq!(
            PromotionStore::live_source_sensitivity(store, "old").unwrap(),
            None
        );
        assert!(!PromotionStore::is_live(store, "old").unwrap());
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[cfg(test)]
macro_rules! promotion_conformance {
    ($make:expr) => {
        use $crate::db::promotions::conformance;

        #[test]
        fn promotion_reads_see_the_corpus_as_the_rules_say() {
            conformance::promotion_reads_see_the_corpus_as_the_rules_say(&mut $make);
        }
        #[test]
        fn surviving_sources_counts_only_live_unsuperseded_sources() {
            conformance::surviving_sources_counts_only_live_unsuperseded_sources(&mut $make);
        }
        #[test]
        fn live_source_sensitivity_is_none_for_a_superseded_source() {
            conformance::live_source_sensitivity_is_none_for_a_superseded_source(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use promotion_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::curation::conformance::{seed_corpus, NOW};
    use crate::db::Database;

    /// The facade locks per call and reaches the engine through the port.
    #[test]
    fn the_facade_reads_back_what_it_records() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed_corpus(&mut *store.backend().lock().unwrap());
        let promotions = Promotions::new(&store);
        promotions.record("sc1", "f4", "scenario", NOW).unwrap();
        assert_eq!(promotions.sources_at("sc1", "scenario").unwrap(), ["f4"]);
        assert_eq!(promotions.derived_from("f4").unwrap(), ["sc1"]);
        assert_eq!(promotions.count_undecomposed_dialogs().unwrap(), 1);
        assert_eq!(promotions.statements_newest_first("fact").unwrap().len(), 3);
    }
}
