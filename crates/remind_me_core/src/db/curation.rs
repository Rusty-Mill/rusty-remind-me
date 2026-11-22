//! Storage reads for the curation queues: captures awaiting decomposition,
//! raw imports awaiting normalization, contradiction candidates, and the
//! maintenance counts over all of them, behind the [`CurationStore`] port
//! (ADR-0035), which the engine's memories core implements in
//! `db::engine::curation`.
//!
//! Every read [`crate::capture`], [`crate::normalize`],
//! [`crate::maintenance`] and [`crate::contradictions`] make goes through
//! here. The rules stay there: snippet lengths, batch bounds, which sources
//! count as raw imports, the entity fan-out ceiling, and the keyset
//! cursor's meaning.

use super::backend::BackendLock;
use super::{Result, Store};
use crate::models::{ContradictionSide, Memory};

/// A capture that nothing has been decomposed from yet.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRow {
    pub id: String,
    pub capture_id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    /// The newest `updated_at` among the capture's rows.
    pub revision: String,
}

/// A raw import that nothing has been normalized from yet.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportRow {
    pub id: String,
    pub content: String,
    pub category: String,
    pub source: String,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
}

/// What a normalization copies from the memory it distils.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizationSource {
    pub tags: Vec<String>,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
}

/// One maintenance backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backlog {
    /// Captures with no decomposed facts pointing back at them.
    Undecomposed,
    /// Live memories, other than raw dialogs, with neither a triple nor an
    /// entity mention.
    Unannotated,
    /// Live raw imports nothing names as `normalized_from`.
    Unnormalized,
    /// Live memories whose `memory_type` is still `unclassified`.
    Unclassified,
}

/// How many distinct captures there are, and when the newest was written.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureActivity {
    pub captures: i64,
    pub last_capture_at: Option<String>,
}

/// The port for the curation reads over the memories core (ADR-0035).
/// Every method reads. [`conformance`] holds the behaviour every
/// implementation must show.
pub trait CurationStore {
    /// Every memory carrying `capture_id`, by category (ties by id).
    fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>>;

    /// The tags of the lowest-id memory carrying `capture_id`, or `None` when
    /// none does. Unparseable tags read as none.
    fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>>;

    /// Captures due for decomposition, newest first, at most `limit`; with
    /// `settled_before`, only those last changed at or before it (RFC 3339).
    fn undecomposed(&self, limit: usize, settled_before: Option<&str>) -> Result<Vec<CaptureRow>>;

    /// How many captures [`CurationStore::undecomposed`] would list.
    fn count_undecomposed(&self, settled_before: Option<&str>) -> Result<i64>;

    /// How many distinct live captures there are, and the newest one's
    /// `created_at`.
    fn capture_activity(&self) -> Result<CaptureActivity>;

    /// Live raw imports from `sources` nothing has been normalized from,
    /// newest first, at most `limit`. Unparseable metadata reads as `{}`.
    fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>>;

    /// How many live raw imports from `sources` nothing has been
    /// normalized from.
    fn count_unnormalized(&self, sources: &[&str]) -> Result<i64>;

    /// What a normalization of `memory_id` copies from it, if it exists.
    fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>>;

    /// How deep `backlog` is.
    fn backlog_depth(&self, backlog: Backlog) -> Result<i64>;

    /// How many contradiction candidate pairs there are: pairs of live,
    /// non-dialog memories sharing an entity mentioned at most `max_fanout`
    /// times, excluding pairs with the same subject and predicate.
    fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64>;

    /// Contradiction candidate pairs in `(id_a, id_b)` order, after `cursor`
    /// when given, at most `limit`.
    fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>>;

    /// The standing decisions that share a claim, or a non-hub entity, with
    /// `memory_id`, in id order, each with the shared entities' names.
    fn decision_neighbours(
        &self,
        memory_id: &str,
        max_fanout: i64,
    ) -> Result<Vec<(String, Vec<String>)>>;

    /// The names of the entities both memories mention, by name.
    fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>>;

    /// One side of a contradiction pair, with the first `snippet_chars`
    /// characters of its content.
    fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide>;
}

/// The curation reads, on the engine's memories core.
pub struct Curation<'c> {
    backend: &'c BackendLock,
}

impl<'c> Curation<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`CurationStore::capture_rows`].
    pub fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        CurationStore::capture_rows(&*self.backend.lock()?, capture_id)
    }

    /// See [`CurationStore::capture_tags`].
    pub fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        CurationStore::capture_tags(&*self.backend.lock()?, capture_id)
    }

    /// See [`CurationStore::undecomposed`].
    pub fn undecomposed(
        &self,
        limit: usize,
        settled_before: Option<&str>,
    ) -> Result<Vec<CaptureRow>> {
        CurationStore::undecomposed(&*self.backend.lock()?, limit, settled_before)
    }

    /// See [`CurationStore::count_undecomposed`].
    pub fn count_undecomposed(&self, settled_before: Option<&str>) -> Result<i64> {
        CurationStore::count_undecomposed(&*self.backend.lock()?, settled_before)
    }

    /// See [`CurationStore::capture_activity`].
    pub fn capture_activity(&self) -> Result<CaptureActivity> {
        CurationStore::capture_activity(&*self.backend.lock()?)
    }

    /// See [`CurationStore::unnormalized`].
    pub fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>> {
        CurationStore::unnormalized(&*self.backend.lock()?, sources, limit)
    }

    /// See [`CurationStore::count_unnormalized`].
    pub fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        CurationStore::count_unnormalized(&*self.backend.lock()?, sources)
    }

    /// See [`CurationStore::normalization_source`].
    pub fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        CurationStore::normalization_source(&*self.backend.lock()?, memory_id)
    }

    /// See [`CurationStore::backlog_depth`].
    pub fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        CurationStore::backlog_depth(&*self.backend.lock()?, backlog)
    }

    /// See [`CurationStore::count_contradiction_pairs`].
    pub fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64> {
        CurationStore::count_contradiction_pairs(&*self.backend.lock()?, max_fanout)
    }

    /// See [`CurationStore::contradiction_pairs`].
    pub fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        CurationStore::contradiction_pairs(&*self.backend.lock()?, max_fanout, cursor, limit)
    }

    /// See [`CurationStore::decision_neighbours`].
    pub fn decision_neighbours(
        &self,
        memory_id: &str,
        max_fanout: i64,
    ) -> Result<Vec<(String, Vec<String>)>> {
        CurationStore::decision_neighbours(&*self.backend.lock()?, memory_id, max_fanout)
    }

    /// See [`CurationStore::shared_entity_names`].
    pub fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        CurationStore::shared_entity_names(&*self.backend.lock()?, id_a, id_b)
    }

    /// See [`CurationStore::contradiction_side`].
    pub fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        CurationStore::contradiction_side(&*self.backend.lock()?, memory_id, snippet_chars)
    }
}

/// The behaviour every [`CurationStore`] must show, run against each backend
/// by [`curation_conformance`] (ADR-0035).
///
/// The reads are over memories and their entity links, so a backend under
/// test implements [`MemoryStore`] and [`EntityStore`] too, and the checks
/// seed through them.
#[doc(hidden)]
pub mod conformance {
    use super::{Backlog, CaptureActivity, CurationStore, NormalizationSource};
    use crate::db::derived::Origin;
    use crate::db::entities::EntityStore;
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::entity::Entity;
    use crate::models::Memory;

    pub const NOW: &str = "2026-09-26T00:00:00+00:00";
    pub const T2: &str = "2026-09-27T00:00:00+00:00";

    /// Captures, imports, facts with triples, scenarios and personas, linked
    /// to three entities, with `f2` superseded by `f1`.
    pub fn seed_corpus<S: MemoryStore + EntityStore + ?Sized>(store: &mut S) {
        let text = |s: &str| Some(s.to_string());
        let rows: Vec<NewMemory> = vec![
            NewMemory {
                capture_id: text("c1"),
                category: "dialog".into(),
                tags: vec!["x".into()],
                ..NewMemory::new("d1", "dialog one", NOW)
            },
            NewMemory {
                capture_id: text("c1"),
                category: "summary".into(),
                ..NewMemory::new("s1", "summary one", NOW)
            },
            NewMemory {
                capture_id: text("c2"),
                category: "dialog".into(),
                ..NewMemory::new("d2", "dialog two", T2)
            },
            NewMemory {
                source_capture_id: text("c2"),
                category: "fact".into(),
                subject: text(" Rust "),
                predicate: text("IS"),
                object: text("fast"),
                vitality: 0.9,
                ..NewMemory::new("f1", "fact one ü", T2)
            },
            NewMemory {
                category: "fact".into(),
                subject: text("rust"),
                predicate: text("is"),
                object: text("safe"),
                vitality: 0.9,
                ..NewMemory::new("f2", "fact two", NOW)
            },
            NewMemory {
                category: "fact".into(),
                vitality: 0.4,
                sensitive: true,
                ..NewMemory::new("f3", "fact three", NOW)
            },
            NewMemory {
                category: "fact".into(),
                vitality: 0.7,
                ..NewMemory::new("f4", "fact four", T2)
            },
            NewMemory {
                source: "chat_import".into(),
                metadata: serde_json::json!({"k": 1}),
                ..NewMemory::new("raw1", "raw one", NOW)
            },
            NewMemory {
                source: "document_import".into(),
                doc_id: text("doc"),
                chunk_index: Some(2),
                ..NewMemory::new("raw2", "raw two", T2)
            },
            NewMemory {
                metadata: serde_json::json!({"normalized_from": "raw1"}),
                ..NewMemory::new("n1", "norm", T2)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.8,
                ..NewMemory::new("sc1", "scenario one", NOW)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.8,
                ..NewMemory::new("sc2", "scenario two", NOW)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.2,
                ..NewMemory::new("sc3", "scenario three", NOW)
            },
            NewMemory {
                category: "persona".into(),
                vitality: 0.5,
                sensitive: true,
                ..NewMemory::new("p1", "persona", T2)
            },
        ];
        for row in &rows {
            MemoryStore::insert(store, row).unwrap();
        }
        for (id, name) in [("e1", "Rust"), ("e2", "Tokio"), ("e3", "Hub")] {
            EntityStore::insert(
                store,
                &Entity {
                    id: id.into(),
                    name: name.into(),
                    kind: None,
                    aliases: vec![],
                    created_at: NOW.into(),
                    updated_at: NOW.into(),
                },
                None,
            )
            .unwrap();
        }
        for (m, e) in [
            ("f1", "e1"),
            ("f2", "e1"),
            ("f3", "e1"),
            ("f4", "e1"),
            ("f1", "e2"),
            ("f4", "e2"),
            ("d1", "e2"),
            ("f2", "e3"),
            ("f4", "e3"),
            ("sc1", "e3"),
            ("ghost", "e3"),
        ] {
            EntityStore::link(store, m, e, NOW, Origin::Local).unwrap();
        }
        MemoryStore::set_superseded_by(store, "f2", "f1", None).unwrap();
    }

    /// Every curation read over [`seed_corpus`].
    pub fn curation_reads_see_the_corpus_as_the_rules_say<
        S: CurationStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        seed_corpus(store);
        let text = |s: &str| Some(s.to_string());
        let sources = ["document_import", "chat_import"];
        let ids =
            |memories: Vec<Memory>| -> Vec<String> { memories.into_iter().map(|m| m.id).collect() };
        assert_eq!(ids(store.capture_rows("c1").unwrap()), ["d1", "s1"]);
        assert_eq!(
            store.capture_tags("c1").unwrap(),
            Some(vec!["x".to_string()])
        );
        assert_eq!(store.capture_tags("none").unwrap(), None);
        let undecomposed: Vec<String> = store
            .undecomposed(10, None)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            undecomposed,
            ["s1", "d1"],
            "c2 was decomposed into f1; c1's two rows wait, newest first then by id"
        );
        assert_eq!(store.count_undecomposed(None).unwrap(), 2);
        assert_eq!(
            store.capture_activity().unwrap(),
            CaptureActivity {
                captures: 2,
                last_capture_at: Some(T2.to_string()),
            }
        );
        let unnormalized = store.unnormalized(&sources, 10).unwrap();
        assert_eq!(unnormalized.len(), 1, "{unnormalized:?}");
        assert_eq!(unnormalized[0].id, "raw2");
        assert_eq!(unnormalized[0].metadata, serde_json::json!({}));
        assert_eq!(store.count_unnormalized(&sources).unwrap(), 1);
        assert_eq!(
            store.normalization_source("raw2").unwrap(),
            Some(NormalizationSource {
                tags: vec![],
                doc_id: text("doc"),
                chunk_index: Some(2),
            })
        );
        assert_eq!(store.normalization_source("missing").unwrap(), None);
        assert_eq!(store.backlog_depth(Backlog::Undecomposed).unwrap(), 2);
        assert_eq!(store.backlog_depth(Backlog::Unnormalized).unwrap(), 1);
        assert_eq!(
            store.backlog_depth(Backlog::Unclassified).unwrap(),
            14,
            "every live memory is unclassified"
        );
        // Unannotated: live, not dialog, no triple, no links.
        assert_eq!(store.backlog_depth(Backlog::Unannotated).unwrap(), 7);

        let pairs_at_20 = store.count_contradiction_pairs(20).unwrap();
        assert!(pairs_at_20 > 0, "the corpus has contradiction pairs");
        assert_eq!(
            store.contradiction_pairs(20, None, 2).unwrap(),
            [
                ("f1".to_string(), "f3".to_string()),
                ("f1".to_string(), "f4".to_string())
            ]
        );
        let after = store
            .contradiction_pairs(20, Some(("f1", "f3")), 10)
            .unwrap();
        assert_eq!(after.len() as i64, pairs_at_20 - 1);
        assert!(after
            .iter()
            .all(|(a, b)| (a.as_str(), b.as_str()) > ("f1", "f3")));
        assert!(
            store.count_contradiction_pairs(3).unwrap() < pairs_at_20,
            "a lower fan-out ceiling drops e1's pairs"
        );
        assert_eq!(
            store.shared_entity_names("f1", "f4").unwrap(),
            ["Rust", "Tokio"]
        );
        let side = store.contradiction_side("f1", 5).unwrap();
        assert_eq!(side.content_snippet, "fact ");
        assert_eq!(side.subject.as_deref(), Some(" Rust "));
        assert!(store.contradiction_side("missing", 5).is_err());
    }

    pub fn a_capture_leaves_the_backlog_once_a_fact_names_it<
        S: CurationStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(
            store,
            &NewMemory {
                capture_id: Some("cap".to_string()),
                ..NewMemory::new("dialog", "x", NOW)
            },
        )
        .unwrap();
        assert_eq!(CurationStore::count_undecomposed(store, None).unwrap(), 1);
        assert_eq!(
            CurationStore::backlog_depth(store, Backlog::Undecomposed).unwrap(),
            1
        );

        MemoryStore::insert(
            store,
            &NewMemory {
                source_capture_id: Some("cap".to_string()),
                ..NewMemory::new("fact", "y", NOW)
            },
        )
        .unwrap();
        assert_eq!(CurationStore::count_undecomposed(store, None).unwrap(), 0);
        assert!(CurationStore::undecomposed(store, 10, None)
            .unwrap()
            .is_empty());
        assert_eq!(
            CurationStore::backlog_depth(store, Backlog::Undecomposed).unwrap(),
            0
        );
    }

    pub fn an_import_leaves_the_backlog_once_normalized<
        S: CurationStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(
            store,
            &NewMemory {
                source: "chat_import".to_string(),
                ..NewMemory::new("raw", "x", NOW)
            },
        )
        .unwrap();
        let sources = ["document_import", "chat_import"];
        assert_eq!(
            CurationStore::count_unnormalized(store, &sources).unwrap(),
            1
        );
        assert_eq!(
            CurationStore::backlog_depth(store, Backlog::Unnormalized).unwrap(),
            1
        );

        MemoryStore::insert(
            store,
            &NewMemory {
                metadata: serde_json::json!({ "normalized_from": "raw" }),
                ..NewMemory::new("norm", "y", NOW)
            },
        )
        .unwrap();
        assert_eq!(
            CurationStore::count_unnormalized(store, &sources).unwrap(),
            0
        );
        assert_eq!(
            CurationStore::backlog_depth(store, Backlog::Unnormalized).unwrap(),
            0
        );
    }

    pub fn contradiction_pairs_page_by_keyset<S: CurationStore + MemoryStore + EntityStore>(
        store: &mut S,
    ) {
        for id in ["a", "b", "c"] {
            MemoryStore::insert(store, &NewMemory::new(id, id, NOW)).unwrap();
            EntityStore::link(store, id, "e", NOW, Origin::Local).unwrap();
        }
        assert_eq!(
            CurationStore::count_contradiction_pairs(store, 20).unwrap(),
            3
        );

        let first = CurationStore::contradiction_pairs(store, 20, None, 2).unwrap();
        assert_eq!(
            first,
            vec![
                ("a".to_string(), "b".to_string()),
                ("a".to_string(), "c".to_string())
            ]
        );
        let rest = CurationStore::contradiction_pairs(store, 20, Some(("a", "c")), 2).unwrap();
        assert_eq!(rest, vec![("b".to_string(), "c".to_string())]);
        assert_eq!(
            CurationStore::count_contradiction_pairs(store, 2).unwrap(),
            0
        );
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[doc(hidden)]
#[macro_export]
macro_rules! curation_conformance {
    ($make:expr) => {
        use $crate::db::curation::conformance;

        #[test]
        fn curation_reads_see_the_corpus_as_the_rules_say() {
            conformance::curation_reads_see_the_corpus_as_the_rules_say(&mut $make);
        }
        #[test]
        fn a_capture_leaves_the_backlog_once_a_fact_names_it() {
            conformance::a_capture_leaves_the_backlog_once_a_fact_names_it(&mut $make);
        }
        #[test]
        fn an_import_leaves_the_backlog_once_normalized() {
            conformance::an_import_leaves_the_backlog_once_normalized(&mut $make);
        }
        #[test]
        fn contradiction_pairs_page_by_keyset() {
            conformance::contradiction_pairs_page_by_keyset(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::curation_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// The facade locks per call and reaches the engine through the port.
    #[test]
    fn the_facade_reads_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        conformance::seed_corpus(&mut *store.backend().lock().unwrap());
        let curation = Curation::new(&store);
        assert_eq!(curation.count_undecomposed(None).unwrap(), 2);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 1);
        assert_eq!(curation.capture_rows("c1").unwrap().len(), 2);
        assert!(curation.contradiction_side("missing", 5).is_err());
    }
}
