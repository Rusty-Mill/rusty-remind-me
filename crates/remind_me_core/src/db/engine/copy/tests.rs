//! The copy's tests: the SQLite stores under `tests/fixtures/legacy_store`
//! copied onto the engine must read back through every repository as the
//! rows were written.
//!
//! The fixtures were written by the last build that still stored in SQLite
//! (schema v32, and the same store downgraded to v31). They hold a row in
//! every table, including awkward ones: a sensitive memory with a reminder,
//! a superseded chunk, a tombstone, a sent outbox entry, a float that fast
//! parsers read back one bit off.

use super::*;
use crate::db::archives::Archives;
use crate::db::entities::Entities;
use crate::db::feedback::Feedback;
use crate::db::history::Revisions;
use crate::db::imports::ImportLedger;
use crate::db::legacy_sqlite::{fixture, LegacyDb};
use crate::db::memories::Memories;
use crate::db::outbox::Outbox;
use crate::db::promotions::Promotions;
use crate::db::related::Related;
use crate::db::saved_searches::SavedSearches;
use crate::db::stats::StoreStats;
use crate::db::sync_state::SyncState;
use crate::db::vectors::Vectors;
use crate::db::wiki::WikiIndex;
use crate::db::Database;
use crate::testing::{self, Table};
use std::path::{Path, PathBuf};

const T1: &str = "2026-01-01T00:00:00+00:00";
const T2: &str = "2026-02-01T00:00:00+00:00";

/// The fixture at `tests/fixtures/legacy_store/<name>`.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/legacy_store")
        .join(name)
}

/// A copy of the fixture `name` in a scratch directory, so a test can
/// alter it without touching the checked-in file.
fn scratch_copy(name: &str, test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "remind_me_copy_{test}_{}_{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memory.db");
    std::fs::copy(fixture_path(name), &path).unwrap();
    path
}

/// A database on the engine holding a copy of the whole of the SQLite file
/// at `source`.
fn copied_store(source: &Path) -> (Database, CopyReport) {
    let source = LegacyDb::open(source).unwrap();
    let target = Database::open_in_memory().unwrap();
    let report = {
        let store = target.store();
        let mut done = Vec::new();
        let report = copy_store(&source, &mut store.engine().lock(), &mut |table| {
            done.push(table)
        })
        .unwrap();
        // Progress names every table the report does, once each and with
        // the same count, as the copy finishes it.
        let reported: Vec<(&str, usize)> = done.iter().map(|d| (d.table, d.rows)).collect();
        let mut sorted = reported.clone();
        sorted.sort_unstable();
        let expected: Vec<(&str, usize)> = report.copied.iter().map(|(t, n)| (*t, *n)).collect();
        assert_eq!(sorted, expected, "progress {reported:?}");
        report
    };
    (target, report)
}

/// A database on the engine whose core alone holds a copy of `source`.
fn copied_core(source: &Path) -> (Database, CopyReport) {
    let source = LegacyDb::open(source).unwrap();
    let target = Database::open_in_memory().unwrap();
    let report = copy_core(&source, &mut target.store().core().lock()).unwrap();
    (target, report)
}

/// Every core table of the fixture, read back through the repositories.
fn check_core(db: &Database) {
    let store = db.store();
    let memories = Memories::new(&store);
    let ids: Vec<String> = ["m1", "m2", "m3"].map(String::from).to_vec();
    let mut got = memories.get_many(&ids).unwrap();
    got.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(got.len(), 3);
    let m1 = &got[0];
    assert_eq!(m1.content, "first memory");
    assert_eq!(m1.tags, ["a", "b"]);
    assert_eq!(m1.metadata, serde_json::json!({"k": [1, 2]}));
    assert!(m1.sensitive);
    assert_eq!(m1.remind_at.as_deref(), Some(T2));
    assert_eq!(m1.capture_id.as_deref(), Some("cap"));
    assert_eq!(m1.vitality, 1.0);
    assert_eq!(m1.confidence, 1.0);
    assert_eq!(m1.written_by, "unknown");
    assert_eq!(m1.capture_method, "manual");
    assert_eq!(m1.project, None);
    let m2 = &got[1];
    assert_eq!(m2.doc_id.as_deref(), Some("doc"));
    assert_eq!(m2.chunk_index, Some(2));
    assert_eq!(m2.superseded_by.as_deref(), Some("m1"));
    assert_eq!(m2.vitality, 0.25);
    let m3 = &got[2];
    assert_eq!(m3.deleted_at.as_deref(), Some(T2));
    assert_eq!(
        (
            m3.subject.as_deref(),
            m3.predicate.as_deref(),
            m3.object.as_deref()
        ),
        (Some("s"), Some("p"), Some("o"))
    );
    let mut live: Vec<String> = memories
        .all_live()
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    live.sort();
    assert_eq!(live, ["m1", "m2"]);
    assert_eq!(testing::memory_ids(&store).unwrap(), ["m1", "m2", "m3"]);
    assert_eq!(
        testing::memory_column(&store, "m1", "sensitive").unwrap(),
        Some(serde_json::Value::from(1))
    );
    assert_eq!(
        testing::memory_text(&store, "m1", "tags")
            .unwrap()
            .as_deref(),
        Some(r#"["a","b"]"#)
    );

    let entities = Entities::new(&store);
    let names: Vec<(String, String, Vec<String>)> = entities
        .all()
        .unwrap()
        .into_iter()
        .map(|e| (e.id, e.name, e.aliases))
        .collect();
    assert_eq!(
        names,
        [
            (
                "e1".to_string(),
                "Ada".to_string(),
                vec!["Ada!".to_string()]
            ),
            (
                "e2".to_string(),
                "Babbage".to_string(),
                vec!["Babbage!".to_string()]
            ),
        ]
    );
    assert_eq!(
        entities.links_oldest_first().unwrap(),
        [
            ("m1".to_string(), "e1".to_string(), T1.to_string()),
            ("m2".to_string(), "e2".to_string(), T2.to_string()),
        ]
    );
    let relations = entities.relations_oldest_first().unwrap();
    assert_eq!(relations.len(), 1);
    assert_eq!(
        (relations[0].id.as_str(), relations[0].relation.as_str()),
        ("r1", "knew")
    );

    // Sync was on while the fixture was written, so every write queued.
    let outbox = Outbox::new(&store);
    assert_eq!(outbox.len().unwrap(), 8);
    let to_hub: Vec<i64> = outbox
        .unsent_to("hub", 0, 100)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(to_hub, [2, 3, 4, 5, 6, 7, 8], "entry 1 was sent to the hub");
    assert_eq!(outbox.unsent_to("peer", 0, 100).unwrap().len(), 8);
    let first = &outbox.unsent_to("peer", 0, 1).unwrap()[0];
    assert_eq!(first.key, "m1");
    let payload: serde_json::Value = serde_json::from_str(&first.payload_json).unwrap();
    assert_eq!(payload["content"], "first memory");
    assert_eq!(payload["sensitive"], 1, "payloads keep the SQLite shape");
    assert_eq!(
        testing::sends(&store).unwrap(),
        [("hub".to_string(), 1, T2.to_string())]
    );
    assert_eq!(
        SyncState::new(&store)
            .flag("sync_enabled")
            .unwrap()
            .as_deref(),
        Some("1")
    );

    let events = Feedback::new(&store).events("m1").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (events[0].signal.as_str(), events[0].magnitude),
        ("helpful", 0.5)
    );
    assert_eq!(testing::feedback_queries(&store, "m1").unwrap(), ["Q T"]);
    let co = Related::new(&store).co_retrieved(&ids).unwrap();
    assert_eq!(co.len(), 1);
    assert_eq!((co[0].memory.id.as_str(), co[0].weight), ("m1", 1));
    assert_eq!(Promotions::new(&store).sources_of("m2").unwrap(), ["m1"]);
    let chunks: Vec<Vec<u8>> = Vectors::new(&store)
        .all()
        .unwrap()
        .into_iter()
        .map(|c| c.embedding)
        .collect();
    assert_eq!(chunks, [vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
    assert_eq!(
        Vectors::new(&store).meta().unwrap(),
        [("model".to_string(), "tiny".to_string())]
    );
    let ledger = ImportLedger::new(&store);
    assert_eq!(
        ledger.chat_import_with_hash("h").unwrap().as_deref(),
        Some("imp")
    );
    let tracked = ledger.dbs_tracked("src", &["x1"]).unwrap();
    assert_eq!(
        tracked[&("src".to_string(), "x1".to_string())].memory_id,
        "m1"
    );
    assert_eq!(ledger.imported_drawers(&["d1"]).unwrap(), ["d1"]);

    for (table, n) in [
        (Table::Memories, 3),
        (Table::SyncOutbox, 8),
        (Table::SyncSends, 1),
        (Table::ReminderDeliveries, 1),
        (Table::MemoryFeedback, 1),
        (Table::Entities, 2),
        (Table::MemoryEntities, 2),
        (Table::EntityRelations, 1),
        (Table::MemoryAssociations, 1),
        (Table::Promotions, 1),
        (Table::VecChunks, 2),
        (Table::ChatImports, 1),
        (Table::DbsImports, 1),
        (Table::MempalaceImports, 1),
    ] {
        assert_eq!(testing::count(&store, table).unwrap(), n, "{table:?}");
    }
}

/// Every group outside the core, read back through its repository.
fn check_groups(db: &Database) {
    let store = db.store();
    let searches = SavedSearches::new(&store);
    let listed = searches.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].id.as_str(), listed[0].name.as_str()),
        ("ss1", "watched")
    );
    assert!(listed[0].watch);
    assert_eq!(listed[0].filters.category.as_deref(), Some("general"));
    assert!(listed[0].filters.include_sensitive);
    let mut seen: Vec<String> = searches.seen_ids("ss1").unwrap().into_iter().collect();
    seen.sort();
    assert_eq!(seen, ["m1", "m2"]);
    let archives = Archives::new(&store);
    let rows = archives.oldest_first().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].import_id.as_str(), rows[0].byte_len), ("imp", 42));
    let span = archives.span_source("m2").unwrap().unwrap();
    assert_eq!((span.byte_start, span.byte_end), (10, 42));
    assert_eq!(archives.span_count(None).unwrap(), 2);
    let hub = SyncState::new(&store).remote_row("hub").unwrap().unwrap();
    assert_eq!(
        (
            hub.last_pull.as_str(),
            hub.last_pull_id.as_str(),
            hub.last_pull_seq
        ),
        (T1, "m1", 7)
    );
    let stats = StoreStats::new(&store);
    let snapshots = stats.snapshots().unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(
        (snapshots[0].total_memories, snapshots[1].total_memories),
        (3, 5)
    );
    assert_eq!(stats.snapshot_on("2026-02-01").unwrap(), Some(2));
    let revisions = Revisions::new(&store);
    let listed = revisions.list("m1", 10).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].content, format!("before {T2}"));
    assert_eq!(listed[0].sensitive, None);
    assert_eq!(listed[1].sensitive, Some(true));
    assert_eq!(
        revisions
            .revision("m1", listed[1].id)
            .unwrap()
            .map(|t| t.content),
        Some(format!("before {T1}"))
    );
    let wiki = WikiIndex::new(&store);
    let pages: Vec<(String, f64)> = wiki
        .recent_first_then_title()
        .unwrap()
        .into_iter()
        .map(|p| (p.slug, p.mtime))
        .collect();
    assert_eq!(
        pages,
        [("alpha".to_string(), 12.5), ("beta".to_string(), 12.5)]
    );
    assert_eq!(wiki.link_count("alpha").unwrap(), 2);
    let hits: Vec<String> = wiki
        .search(&["quokkas".to_string()], 10)
        .unwrap()
        .into_iter()
        .map(|h| h.snippet)
        .collect();
    assert_eq!(
        hits,
        [
            "Alpha page mentions [quokkas]",
            "Beta page mentions [quokkas]"
        ]
    );
    assert_eq!(wiki.meta("compiled_at").unwrap().as_deref(), Some(T2));
}

#[test]
fn a_copied_v32_store_reads_back_as_it_was_written() {
    let (target, report) = copied_store(&fixture_path("legacy_v32.db"));
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    for (table, n) in [
        ("memories", 3),
        ("sync_outbox", 8),
        ("saved_searches", 1),
        ("saved_search_seen_memories", 2),
        ("import_archives", 1),
        ("import_archive_spans", 2),
        ("sync_log", 1),
        ("analytics_snapshots", 2),
        ("memory_revisions", 2),
        ("wiki_pages", 2),
        ("wiki_links", 2),
        ("wiki_meta", 1),
    ] {
        assert_eq!(report.copied[table], n, "{table}");
    }
    check_core(&target);
    check_groups(&target);
}

/// A v31 store has none of the v32 columns; the copy lands every row with
/// them at their defaults.
#[test]
fn a_v31_store_is_copied_with_the_new_columns_defaulted() {
    let source = LegacyDb::open(&fixture_path("legacy_v31.db")).unwrap();
    assert_eq!(source.user_version().unwrap(), 31);
    let (target, report) = copied_store(&fixture_path("legacy_v31.db"));
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    check_core(&target);
    check_groups(&target);
}

#[test]
fn a_copied_core_alone_reads_back_too() {
    let (target, report) = copied_core(&fixture_path("legacy_v32.db"));
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.copied["memories"], 3);
    assert!(!report.copied.contains_key("wiki_pages"));
    check_core(&target);
}

/// A value decay leaves behind whose shortest decimal form a fast float
/// parser reads back one bit off (as 0.9775).
const INEXACT: f64 = 0.9774999999999999;

#[test]
fn a_float_that_parses_inexactly_is_copied_and_read_back_exactly() {
    let path = scratch_copy("legacy_v32.db", "inexact");
    fixture(
        &path,
        &format!("UPDATE memories SET vitality = {INEXACT:?}, base_weight = {INEXACT:?} WHERE id = 'm1';"),
    )
    .unwrap();
    let (target, report) = copied_core(&path);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let copied = Memories::new(&target.store())
        .get_many(&["m1".to_string()])
        .unwrap();
    assert_eq!(copied[0].vitality.to_bits(), INEXACT.to_bits());
    assert_eq!(copied[0].base_weight.to_bits(), INEXACT.to_bits());
}

#[test]
fn new_ids_continue_past_the_copied_ones() {
    let (target, _) = copied_store(&fixture_path("legacy_v32.db"));
    let store = target.store();
    let queued = testing::queue_outbox(&store, "k", "insert", "{}", T2).unwrap();
    assert!(queued > 8, "outbox id {queued} after 8");
    let snapshot = StoreStats::new(&store)
        .insert_snapshot(&crate::models::AnalyticsSnapshot {
            captured_at: "2026-03-01T00:00:00+00:00".into(),
            total_memories: 1,
            vitality_buckets: Default::default(),
            category_counts: Default::default(),
        })
        .unwrap();
    assert!(snapshot > 2, "snapshot id {snapshot}");
    Revisions::new(&store)
        .insert(
            "m1",
            &crate::db::history::Tracked {
                content: "later".into(),
                category: "general".into(),
                tags: "[]".into(),
                metadata: "{}".into(),
                sensitive: None,
            },
            "2026-03-01T00:00:00+00:00",
            None,
        )
        .unwrap();
    let mut ids: Vec<i64> = Revisions::new(&store)
        .list("m1", 10)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "no revision id was reissued: {ids:?}");
}

#[test]
fn rows_the_engine_cannot_keep_are_refused_and_reported() {
    let path = scratch_copy("legacy_v32.db", "refused");
    fixture(
        &path,
        &format!(
            // A NULL where the engine keeps text.
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at, sent_at)
             VALUES ('m1', 'update', '{{}}', '{T2}', NULL);
             -- Two deliveries the engine would key alike: SQLite's own unique
             -- index forbids this, so it goes first, as in a damaged file.
             DROP INDEX idx_reminder_deliveries_memory_remind_at;
             INSERT INTO reminder_deliveries (memory_id, remind_at, delivered_at)
             VALUES ('m1', '{T2}', '{T1}');"
        ),
    )
    .unwrap();
    let (target, report) = copied_core(&path);
    let mut refused: Vec<(&str, bool)> = report
        .refused
        .iter()
        .map(|r| (r.table, r.reason.contains("already taken")))
        .collect();
    refused.sort();
    assert_eq!(
        refused,
        vec![("reminder_deliveries", true), ("sync_outbox", false)],
        "{:?}",
        report.refused
    );
    // Everything else still arrived.
    let store = target.store();
    assert_eq!(testing::count(&store, Table::Memories).unwrap(), 3);
    assert_eq!(
        testing::count(&store, Table::ReminderDeliveries).unwrap(),
        1
    );
}

#[test]
fn the_copy_refuses_a_filled_target_and_a_source_it_cannot_read() {
    let (target, _) = copied_core(&fixture_path("legacy_v32.db"));
    let source = LegacyDb::open(&fixture_path("legacy_v32.db")).unwrap();
    let again = copy_core(&source, &mut target.store().core().lock());
    assert!(matches!(again, Err(StoreError::Invalid(ref why)) if why.contains("not empty")));

    let old = scratch_copy("legacy_v32.db", "old");
    fixture(&old, "PRAGMA user_version = 3;").unwrap();
    let old = LegacyDb::open(&old).unwrap();
    let fresh = Database::open_in_memory().unwrap();
    let refused = copy_core(&old, &mut fresh.store().core().lock());
    assert!(matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("schema version")));
}

#[test]
fn the_copy_never_writes_to_its_source() {
    let path = scratch_copy("legacy_v32.db", "readonly");
    let before = std::fs::read(&path).unwrap();
    let _ = copied_store(&path);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn the_whole_copy_refuses_a_target_with_any_group_filled() {
    let target = Database::open_in_memory().unwrap();
    {
        let store = target.store();
        WikiIndex::new(&store).set_meta("k", "v").unwrap();
    }
    let source = LegacyDb::open(&fixture_path("legacy_v32.db")).unwrap();
    let refused = copy_store(&source, &mut target.store().engine().lock(), &mut |_| {});
    assert!(matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("not empty")));
}
