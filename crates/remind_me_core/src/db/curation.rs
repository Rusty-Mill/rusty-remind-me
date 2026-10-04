//! Storage reads for the curation queues: captures awaiting decomposition,
//! raw imports awaiting normalization, contradiction candidates, and the
//! maintenance counts over all of them, on the engine's memories core
//! (`db::engine::curation`).
//!
//! Every read [`crate::capture`], [`crate::normalize`],
//! [`crate::maintenance`] and [`crate::contradictions`] make goes through
//! here. The rules stay there: snippet lengths, batch bounds, which sources
//! count as raw imports, the entity fan-out ceiling, and the keyset
//! cursor's meaning.

use super::engine::{self, EngineLock};
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

/// The curation reads, on the engine's memories core.
pub struct Curation<'c> {
    core: &'c EngineLock,
}

impl<'c> Curation<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    // --- captures --------------------------------------------------------

    /// Every memory carrying `capture_id`, by category (ties by id).
    pub fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        engine::curation::capture_rows(&self.core.lock(), capture_id)
    }

    /// The tags of the lowest-id memory carrying `capture_id`, or `None` when
    /// none does. Unparseable tags read as none.
    pub fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        engine::curation::capture_tags(&self.core.lock(), capture_id)
    }

    /// Captures nothing has been decomposed from, newest first, at most
    /// `limit`.
    pub fn undecomposed(&self, limit: usize) -> Result<Vec<CaptureRow>> {
        engine::curation::undecomposed(&self.core.lock(), limit)
    }

    /// How many captures nothing has been decomposed from.
    pub fn count_undecomposed(&self) -> Result<i64> {
        engine::curation::count_undecomposed(&self.core.lock())
    }

    /// How many distinct live captures there are, and the newest one's
    /// `created_at`.
    pub fn capture_activity(&self) -> Result<CaptureActivity> {
        engine::curation::capture_activity(&self.core.lock())
    }

    // --- normalization ---------------------------------------------------

    /// Live raw imports from `sources` nothing has been normalized from,
    /// newest first, at most `limit`. Unparseable metadata reads as `{}`.
    pub fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>> {
        engine::curation::unnormalized(&self.core.lock(), sources, limit)
    }

    /// How many live raw imports from `sources` nothing has been
    /// normalized from.
    pub fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        engine::curation::count_unnormalized(&self.core.lock(), sources)
    }

    /// What a normalization of `memory_id` copies from it, if it exists.
    pub fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        engine::curation::normalization_source(&self.core.lock(), memory_id)
    }

    // --- maintenance -----------------------------------------------------

    /// How deep `backlog` is.
    pub fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        engine::curation::backlog_depth(&self.core.lock(), backlog)
    }

    // --- contradictions --------------------------------------------------

    /// How many contradiction candidate pairs there are: pairs of live,
    /// non-dialog memories sharing an entity mentioned at most `max_fanout`
    /// times, excluding pairs with the same subject and predicate.
    pub fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64> {
        engine::curation::count_contradiction_pairs(&self.core.lock(), max_fanout)
    }

    /// Contradiction candidate pairs in `(id_a, id_b)` order, after `cursor`
    /// when given, at most `limit`.
    pub fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        engine::curation::contradiction_pairs(&self.core.lock(), max_fanout, cursor, limit)
    }

    /// The names of the entities both memories mention, by name.
    pub fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        engine::curation::shared_entity_names(&self.core.lock(), id_a, id_b)
    }

    /// One side of a contradiction pair, with the first `snippet_chars`
    /// characters of its content.
    pub fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        engine::curation::contradiction_side(&self.core.lock(), memory_id, snippet_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Captures, imports, graph links and promotions, and every curation and
    /// promotion read over them.
    #[test]
    fn curation_and_promotion_reads_see_the_corpus_as_the_rules_say() {
        use crate::db::derived::Origin;
        use crate::db::entities::Entities;
        use crate::db::promotions::Promotions;
        use crate::entity::Entity;
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
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
            memories.insert(row).unwrap();
        }
        let entities = Entities::new(&store);
        for (id, name) in [("e1", "Rust"), ("e2", "Tokio"), ("e3", "Hub")] {
            entities
                .insert(
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
            entities.link(m, e, NOW, Origin::Local).unwrap();
        }
        let promotions = Promotions::new(&store);
        for (p, src, rung) in [
            ("sc1", "f4", "scenario"),
            ("sc1", "f2", "scenario"),
            ("sc1", "f2", "scenario"),
            ("p1", "sc1", "persona"),
            ("p1", "gone", "persona"),
        ] {
            promotions.record(p, src, rung, NOW).unwrap();
        }
        memories.set_superseded_by("f2", "f1", None).unwrap();

        let curation = Curation::new(&store);
        let sources = ["document_import", "chat_import"];
        let ids = |memories: Vec<Memory>| -> Vec<String> {
            memories.into_iter().map(|m| m.id).collect()
        };
        assert_eq!(ids(curation.capture_rows("c1").unwrap()), ["d1", "s1"]);
        assert_eq!(curation.capture_tags("c1").unwrap(), Some(vec!["x".to_string()]));
        assert_eq!(curation.capture_tags("none").unwrap(), None);
        let undecomposed: Vec<String> = curation
            .undecomposed(10)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            undecomposed,
            ["s1", "d1"],
            "c2 was decomposed into f1; c1's two rows wait, newest first then by id"
        );
        assert_eq!(curation.count_undecomposed().unwrap(), 2);
        assert_eq!(
            curation.capture_activity().unwrap(),
            CaptureActivity {
                captures: 2,
                last_capture_at: Some(T2.to_string()),
            }
        );
        let unnormalized = curation.unnormalized(&sources, 10).unwrap();
        assert_eq!(unnormalized.len(), 1, "{unnormalized:?}");
        assert_eq!(unnormalized[0].id, "raw2");
        assert_eq!(unnormalized[0].metadata, serde_json::json!({}));
        assert_eq!(curation.count_unnormalized(&sources).unwrap(), 1);
        assert_eq!(
            curation.normalization_source("raw2").unwrap(),
            Some(NormalizationSource {
                tags: vec![],
                doc_id: text("doc"),
                chunk_index: Some(2),
            })
        );
        assert_eq!(curation.normalization_source("missing").unwrap(), None);
        assert_eq!(curation.backlog_depth(Backlog::Undecomposed).unwrap(), 2);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 1);
        assert_eq!(
            curation.backlog_depth(Backlog::Unclassified).unwrap(),
            14,
            "every live memory is unclassified"
        );
        // Unannotated: live, not dialog, no triple, no links.
        assert_eq!(curation.backlog_depth(Backlog::Unannotated).unwrap(), 7);

        let pairs_at_20 = curation.count_contradiction_pairs(20).unwrap();
        assert!(pairs_at_20 > 0, "the corpus has contradiction pairs");
        assert_eq!(
            curation.contradiction_pairs(20, None, 2).unwrap(),
            [("f1".to_string(), "f3".to_string()), ("f1".to_string(), "f4".to_string())]
        );
        let after = curation
            .contradiction_pairs(20, Some(("f1", "f3")), 10)
            .unwrap();
        assert_eq!(after.len() as i64, pairs_at_20 - 1);
        assert!(after.iter().all(|(a, b)| (a.as_str(), b.as_str()) > ("f1", "f3")));
        assert!(
            curation.count_contradiction_pairs(3).unwrap() < pairs_at_20,
            "a lower fan-out ceiling drops e1's pairs"
        );
        assert_eq!(
            curation.shared_entity_names("f1", "f4").unwrap(),
            ["Rust", "Tokio"]
        );
        let side = curation.contradiction_side("f1", 5).unwrap();
        assert_eq!(side.content_snippet, "fact ");
        assert_eq!(side.subject.as_deref(), Some(" Rust "));
        assert!(curation.contradiction_side("missing", 5).is_err());

        let dialogs: Vec<String> = promotions
            .undecomposed_dialogs(10)
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(dialogs, ["d1"]);
        assert_eq!(promotions.count_undecomposed_dialogs().unwrap(), 1);
        let groups = promotions
            .entity_fact_groups("fact", "scenario", 2, 10)
            .unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].entity_name, "Rust");
        assert_eq!(
            groups[0].memory_ids,
            ["f1", "f3"],
            "f2 is superseded and f4 promoted at this rung, which also leaves Tokio one fact"
        );
        assert_eq!(
            promotions
                .entity_fact_groups("fact", "other", 1, 1)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            promotions
                .count_entity_fact_groups("fact", "other", 1)
                .unwrap(),
            3
        );
        let ready: Vec<String> = promotions
            .ready_scenarios("scenario", 0.5, "persona", 10)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ready, ["sc2"], "sc1 is promoted, sc3 too faint");
        assert_eq!(
            promotions
                .count_ready_scenarios("scenario", 0.5, "persona")
                .unwrap(),
            1
        );
        assert_eq!(promotions.live_source_sensitivity("f3").unwrap(), Some(true));
        assert_eq!(promotions.live_source_sensitivity("f2").unwrap(), None);
        assert_eq!(promotions.promoted_from("f2", "scenario").unwrap(), ["sc1"]);
        assert_eq!(promotions.sources_at("sc1", "scenario").unwrap(), ["f2", "f4"]);
        assert!(promotions.is_live("f1").unwrap());
        assert!(!promotions.is_live("f2").unwrap());
        assert_eq!(promotions.sources_of("p1").unwrap(), ["gone", "sc1"]);
        assert_eq!(promotions.derived_from("sc1").unwrap(), ["p1"]);
        assert_eq!(promotions.surviving_sources("sc1").unwrap(), 1);
        let by_vitality: Vec<String> = promotions
            .statements_by_vitality("fact")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(by_vitality, ["f1", "f4"], "non-sensitive, live, unsuperseded");
        let newest: Vec<String> = promotions
            .statements_newest_first("fact")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(newest, ["f1", "f4", "f3"]);
    }

    #[test]
    fn a_capture_leaves_the_backlog_once_a_fact_names_it() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories
            .insert(&NewMemory {
                capture_id: Some("cap".to_string()),
                ..NewMemory::new("dialog", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&store);
        assert_eq!(curation.count_undecomposed().unwrap(), 1);
        assert_eq!(curation.backlog_depth(Backlog::Undecomposed).unwrap(), 1);

        memories
            .insert(&NewMemory {
                source_capture_id: Some("cap".to_string()),
                ..NewMemory::new("fact", "y", NOW)
            })
            .unwrap();
        assert_eq!(curation.count_undecomposed().unwrap(), 0);
        assert!(curation.undecomposed(10).unwrap().is_empty());
        assert_eq!(curation.backlog_depth(Backlog::Undecomposed).unwrap(), 0);
    }

    #[test]
    fn an_import_leaves_the_backlog_once_normalized() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories
            .insert(&NewMemory {
                source: "chat_import".to_string(),
                ..NewMemory::new("raw", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&store);
        let sources = ["document_import", "chat_import"];
        assert_eq!(curation.count_unnormalized(&sources).unwrap(), 1);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 1);

        memories
            .insert(&NewMemory {
                metadata: serde_json::json!({ "normalized_from": "raw" }),
                ..NewMemory::new("norm", "y", NOW)
            })
            .unwrap();
        assert_eq!(curation.count_unnormalized(&sources).unwrap(), 0);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 0);
    }

    #[test]
    fn contradiction_pairs_page_by_keyset() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        let entities = crate::db::entities::Entities::new(&store);
        for id in ["a", "b", "c"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
            entities
                .link(id, "e", NOW, crate::db::derived::Origin::Local)
                .unwrap();
        }
        let curation = Curation::new(&store);
        assert_eq!(curation.count_contradiction_pairs(20).unwrap(), 3);

        let first = curation.contradiction_pairs(20, None, 2).unwrap();
        assert_eq!(
            first,
            vec![
                ("a".to_string(), "b".to_string()),
                ("a".to_string(), "c".to_string())
            ]
        );
        let rest = curation
            .contradiction_pairs(20, Some(("a", "c")), 2)
            .unwrap();
        assert_eq!(rest, vec![("b".to_string(), "c".to_string())]);
        assert_eq!(curation.count_contradiction_pairs(2).unwrap(), 0);
    }
}
