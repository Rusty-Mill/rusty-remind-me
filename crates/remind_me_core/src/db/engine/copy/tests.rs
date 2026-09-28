//! The copy's tests: a rich SQLite store copied onto the engine must read
//! back through every repository exactly as the source does.

use super::*;
use crate::db::derived::Origin;
use crate::db::engine::EngineLock;
use crate::db::entities::{Entities, RelationRow};
use crate::db::feedback::{Feedback, FeedbackEvent};
use crate::db::imports::ImportLedger;
use crate::db::memories::{Memories, NewMemory};
use crate::db::outbox::Outbox;
use crate::db::promotions::{self, Promotions};
use crate::db::related::Related;
use crate::db::reminders::Reminders;
use crate::db::sync_state::SyncState;
use crate::db::vectors::Vectors;
use crate::db::Database;
use crate::entity::Entity;
use crate::testing::{self, Table};

const T1: &str = "2026-01-01T00:00:00+00:00";
const T2: &str = "2026-02-01T00:00:00+00:00";

/// A SQLite store with a row in every core table, including awkward ones.
fn seeded_source() -> Database {
    let db = Database::open_sqlite_in_memory().unwrap();
    let store = db.store();
    SyncState::new(&store)
        .set_flag("sync_enabled", "1")
        .unwrap();
    let memories = Memories::new(&store);
    memories
        .insert(&NewMemory {
            tags: vec!["a".into(), "b".into()],
            metadata: serde_json::json!({"k": [1, 2]}),
            sensitive: true,
            remind_at: Some(T2.into()),
            capture_id: Some("cap".into()),
            ..NewMemory::new("m1", "first memory", T1)
        })
        .unwrap();
    memories
        .insert(&NewMemory {
            doc_id: Some("doc".into()),
            chunk_index: Some(2),
            superseded_by: Some("m1".into()),
            vitality: 0.25,
            ..NewMemory::new("m2", "second", T2)
        })
        .unwrap();
    memories
        .insert(&NewMemory {
            deleted_at: Some(T2.into()),
            subject: Some("s".into()),
            predicate: Some("p".into()),
            object: Some("o".into()),
            ..NewMemory::new("m3", "gone", T1)
        })
        .unwrap();
    let entities = Entities::new(&store);
    for (id, name) in [("e1", "Ada"), ("e2", "Babbage")] {
        entities
            .insert(
                &Entity {
                    id: id.into(),
                    name: name.into(),
                    kind: Some("person".into()),
                    aliases: vec![format!("{name}!")],
                    created_at: T1.into(),
                    updated_at: T1.into(),
                },
                None,
            )
            .unwrap();
    }
    entities.link("m1", "e1", T1, Origin::Local).unwrap();
    entities.link("m2", "e2", T2, Origin::Local).unwrap();
    entities
        .insert_relation_or_ignore(
            &RelationRow {
                id: "r1",
                subject_entity_id: "e1",
                relation: "knew",
                object_entity_id: "e2",
                created_at: T1,
                updated_at: T2,
                node_id: Some("peer"),
            },
            Origin::Local,
        )
        .unwrap();
    let event = FeedbackEvent {
        query_tokens: "q t".into(),
        signal: "helpful".into(),
        magnitude: 0.5,
    };
    Feedback::new(&store)
        .log_event("fb1", "m1", "Q T", &event, T1)
        .unwrap();
    Reminders::new(&store)
        .record_delivery("m1", T2, T2)
        .unwrap();
    Related::new(&store).bump_pair("m1", "m2", T1, 5).unwrap();
    promotions::ensure_table(&store).unwrap();
    Promotions::new(&store)
        .record("m2", "m1", "scenario", T2)
        .unwrap();
    let vectors = Vectors::new(&store);
    vectors.put("m1", 0, &[1, 2, 3, 4]).unwrap();
    vectors.put("m1", 1, &[5, 6, 7, 8]).unwrap();
    vectors.set_meta("model", "tiny", T1).unwrap();
    let ledger = ImportLedger::new(&store);
    ledger
        .record_chat("imp", "a.json", "h", T1, r#"{"n":1}"#)
        .unwrap();
    ledger.record_dbs("src", "x1", "m1", "h1", T1).unwrap();
    ledger.record_mempalace("d1", "m2", T1).unwrap();
    let first = Outbox::new(&store).unsent_to("hub", 0, 1).unwrap()[0].id;
    SyncState::new(&store)
        .record_sends("hub", &[first], T2)
        .unwrap();
    drop(store);
    db
}

/// A database on the engine whose core holds a copy of `source`.
fn copied(source: &Database) -> (Database, CopyReport) {
    let target = Database::open_in_memory_on_engine().unwrap();
    let report = {
        let source = source.store();
        let target = target.store();
        let tables: &EngineLock = target.core().unwrap();
        let report = copy_core(source.sqlite().unwrap(), &mut tables.lock()).unwrap();
        report
    };
    (target, report)
}

/// Every core table, read back through the repositories, as text.
fn read_back(db: &Database) -> Vec<String> {
    let store = db.store();
    let ids: Vec<String> = ["m1", "m2", "m3"].map(String::from).to_vec();
    let mut seen = vec![
        format!("{:?}", Memories::new(&store).get_many(&ids).unwrap()),
        // `all_live` promises no order, so compare it by id.
        format!("{:?}", {
            let mut live = Memories::new(&store).all_live().unwrap();
            live.sort_by(|a, b| a.id.cmp(&b.id));
            live
        }),
        format!("{:?}", testing::memory_ids(&store).unwrap()),
        format!("{:?}", Entities::new(&store).all().unwrap()),
        format!("{:?}", Entities::new(&store).links_oldest_first().unwrap()),
        format!(
            "{:?}",
            Entities::new(&store).relations_oldest_first().unwrap()
        ),
        format!(
            "{:?}",
            Outbox::new(&store).unsent_to("hub", 0, 100).unwrap()
        ),
        format!(
            "{:?}",
            Outbox::new(&store).unsent_to("peer", 0, 100).unwrap()
        ),
        format!("{:?}", testing::sends(&store).unwrap()),
        format!("{:?}", SyncState::new(&store).flag("sync_enabled").unwrap()),
        format!("{:?}", Feedback::new(&store).events("m1").unwrap()),
        format!("{:?}", testing::feedback_queries(&store, "m1").unwrap()),
        format!("{:?}", Related::new(&store).co_retrieved(&ids).unwrap()),
        format!("{:?}", Promotions::new(&store).sources_of("m2").unwrap()),
        format!("{:?}", Vectors::new(&store).all().unwrap()),
        format!("{:?}", Vectors::new(&store).meta().unwrap()),
        format!(
            "{:?}",
            ImportLedger::new(&store)
                .chat_import_with_hash("h")
                .unwrap()
        ),
        format!(
            "{:?}",
            ImportLedger::new(&store)
                .dbs_tracked("src", &["x1"])
                .unwrap()
        ),
        format!(
            "{:?}",
            ImportLedger::new(&store).imported_drawers(&["d1"]).unwrap()
        ),
    ];
    for id in &ids {
        for column in [
            "sensitive",
            "remind_at",
            "tags",
            "metadata",
            "vitality",
            "deleted_at",
        ] {
            seen.push(format!(
                "{id}.{column}={:?}",
                testing::memory_column(&store, id, column).unwrap()
            ));
        }
    }
    for table in [
        Table::Memories,
        Table::SyncOutbox,
        Table::SyncSends,
        Table::ReminderDeliveries,
        Table::MemoryFeedback,
        Table::Entities,
        Table::MemoryEntities,
        Table::EntityRelations,
        Table::MemoryAssociations,
        Table::Promotions,
        Table::VecChunks,
        Table::ChatImports,
        Table::DbsImports,
        Table::MempalaceImports,
    ] {
        seen.push(format!(
            "{table:?}={}",
            testing::count(&store, table).unwrap()
        ));
    }
    seen
}

/// A value decay leaves behind whose shortest decimal form a fast float
/// parser reads back one bit off (as 0.9775).
const INEXACT: f64 = 0.9774999999999999;

#[test]
fn a_float_that_parses_inexactly_is_copied_and_read_back_exactly() {
    let source = Database::open_sqlite_in_memory().unwrap();
    Memories::new(&source.store())
        .insert(&NewMemory {
            vitality: INEXACT,
            base_weight: INEXACT,
            ..NewMemory::new("m1", "decayed", T1)
        })
        .unwrap();

    let (target, report) = copied(&source);

    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let copied = Memories::new(&target.store())
        .get_many(&["m1".to_string()])
        .unwrap();
    assert_eq!(copied[0].vitality.to_bits(), INEXACT.to_bits());
    assert_eq!(copied[0].base_weight.to_bits(), INEXACT.to_bits());
}

#[test]
fn a_copied_core_reads_back_as_the_source_does() {
    let source = seeded_source();
    let (target, report) = copied(&source);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.copied["memories"], 3);
    assert!(report.copied["sync_outbox"] > 0);
    let ours = read_back(&source);
    let theirs = read_back(&target);
    for (a, b) in ours.iter().zip(&theirs) {
        assert_eq!(b, a);
    }
    assert_eq!(theirs.len(), ours.len());
}

#[test]
fn new_outbox_ids_continue_past_the_copied_ones() {
    let source = seeded_source();
    let highest = {
        let store = source.store();
        Outbox::new(&store)
            .unsent_to("nobody", 0, 1000)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .max()
            .unwrap()
    };
    let (target, _) = copied(&source);
    let store = target.store();
    let queued = testing::queue_outbox(&store, "k", "insert", "{}", T2).unwrap();
    assert!(queued > highest, "{queued} after {highest}");
}

#[test]
fn rows_the_engine_cannot_keep_are_refused_and_reported() {
    let source = seeded_source();
    {
        let store = source.store();
        let conn = store.sqlite().unwrap();
        // A NULL where the engine keeps text.
        conn.execute(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at, sent_at)
             VALUES ('m1', 'update', '{}', ?, NULL)",
            [T2],
        )
        .unwrap();
        // Two deliveries the engine would key alike: SQLite's own unique
        // index forbids this, so it goes first, as in a damaged file.
        conn.execute_batch("DROP INDEX idx_reminder_deliveries_memory_remind_at")
            .unwrap();
        conn.execute(
            "INSERT INTO reminder_deliveries (memory_id, remind_at, delivered_at) VALUES ('m1', ?, ?)",
            [T2, T1],
        )
        .unwrap();
    }
    let (target, report) = copied(&source);
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
fn the_copy_refuses_a_filled_target_and_an_old_source() {
    let source = seeded_source();
    let (target, _) = copied(&source);
    {
        let source = source.store();
        let target = target.store();
        let again = copy_core(source.sqlite().unwrap(), &mut target.core().unwrap().lock());
        assert!(matches!(again, Err(StoreError::Invalid(ref why)) if why.contains("not empty")));
    }
    let old = Database::open_sqlite_in_memory().unwrap();
    let old = old.store();
    old.sqlite()
        .unwrap()
        .execute_batch("PRAGMA user_version = 3")
        .unwrap();
    let fresh = Database::open_in_memory_on_engine().unwrap();
    let fresh = fresh.store();
    let refused = copy_core(old.sqlite().unwrap(), &mut fresh.core().unwrap().lock());
    assert!(matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("schema version")));
}

#[test]
fn the_copy_never_writes_to_its_source() {
    let source = seeded_source();
    let before = read_back(&source);
    let changes_before: i64 = source
        .store()
        .sqlite()
        .unwrap()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    let _ = copied(&source);
    let changes_after: i64 = source
        .store()
        .sqlite()
        .unwrap()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(changes_after, changes_before);
    assert_eq!(read_back(&source), before);
}

// --- the other groups -------------------------------------------------------

use crate::db::archives::{self, ArchiveRow, Archives};
use crate::db::history::{Revisions, Tracked};
use crate::db::saved_searches::SavedSearches;
use crate::db::stats::StoreStats;
use crate::db::sync_state::SyncLogRow;
use crate::db::wiki::WikiIndex;
use crate::models::{AnalyticsSnapshot, SavedSearch, SavedSearchFilters};
use crate::wiki::WikiPage;

/// [`seeded_source`], plus a row in every group outside the core.
fn seeded_store() -> Database {
    let db = seeded_source();
    let store = db.store();
    let searches = SavedSearches::new(&store);
    searches
        .insert(&SavedSearch {
            id: "ss1".into(),
            name: "watched".into(),
            query: "memory".into(),
            filters: SavedSearchFilters {
                category: Some("general".into()),
                tags: Some(vec!["a".into()]),
                include_sensitive: true,
            },
            watch: true,
            created_at: T1.into(),
            updated_at: T2.into(),
        })
        .unwrap();
    searches
        .mark_seen("ss1", &["m1".into(), "m2".into()], T2)
        .unwrap();
    archives::ensure_tables(&store).unwrap();
    let archive = Archives::new(&store);
    archive
        .record(&ArchiveRow {
            import_id: "imp".into(),
            hash: "h".into(),
            filename: "a.json".into(),
            archive_path: "archives/h.json".into(),
            byte_len: 42,
            archived_at: T1.into(),
        })
        .unwrap();
    archive.record_span("m1", "imp", 0, 10).unwrap();
    archive.record_span("m2", "imp", 10, 42).unwrap();
    SyncState::new(&store)
        .put_remote_row(&SyncLogRow {
            last_pull: T1.into(),
            last_pull_id: "m1".into(),
            last_pull_seq: 7,
            ..SyncLogRow::new("hub")
        })
        .unwrap();
    let stats = StoreStats::new(&store);
    for (day, total) in [(T1, 3), (T2, 5)] {
        stats
            .insert_snapshot(&AnalyticsSnapshot {
                captured_at: day.into(),
                total_memories: total,
                vitality_buckets: [("high".to_string(), 2)].into(),
                category_counts: [("general".to_string(), total)].into(),
            })
            .unwrap();
    }
    let revisions = Revisions::new(&store);
    for (at, sensitive) in [(T1, Some(true)), (T2, None)] {
        revisions
            .insert(
                "m1",
                &Tracked {
                    content: format!("before {at}"),
                    category: "general".into(),
                    tags: "[]".into(),
                    metadata: "{}".into(),
                    sensitive,
                },
                at,
                Some("edit"),
            )
            .unwrap();
    }
    let wiki = WikiIndex::new(&store);
    for (slug, title) in [("alpha", "Alpha page"), ("beta", "Beta page")] {
        wiki.upsert(&WikiPage {
            slug: slug.into(),
            title: title.into(),
            content: format!("{title} mentions quokkas"),
            summary: format!("about {slug}"),
            mtime: 12.5,
            updated_at: T2.into(),
        })
        .unwrap();
    }
    wiki.replace_links(
        "alpha",
        &[
            ("beta".into(), "Beta page".into()),
            ("gamma".into(), "Gamma".into()),
        ],
    )
    .unwrap();
    wiki.set_meta("compiled_at", T2).unwrap();
    drop(store);
    db
}

/// A database on the engine holding a copy of the whole of `source`.
fn copied_store(source: &Database) -> (Database, CopyReport) {
    let target = Database::open_in_memory_on_engine().unwrap();
    let report = {
        let source = source.store();
        let target = target.store();
        let tables: &EngineLock = target.engine().unwrap();
        let report = copy_store(source.sqlite().unwrap(), &mut tables.lock()).unwrap();
        report
    };
    (target, report)
}

/// Every group outside the core, read back through its repository.
fn read_back_groups(db: &Database) -> Vec<String> {
    let store = db.store();
    let searches = SavedSearches::new(&store);
    let mut seen: Vec<String> = searches.seen_ids("ss1").unwrap().into_iter().collect();
    seen.sort();
    let revisions = Revisions::new(&store);
    let listed = revisions.list("m1", 10).unwrap();
    vec![
        format!("{:?}", searches.list().unwrap()),
        format!("{seen:?}"),
        format!("{:?}", Archives::new(&store).oldest_first().unwrap()),
        format!("{:?}", Archives::new(&store).span_source("m2").unwrap()),
        format!("{:?}", Archives::new(&store).span_count(None).unwrap()),
        format!("{:?}", SyncState::new(&store).remote_row("hub").unwrap()),
        format!("{:?}", StoreStats::new(&store).snapshots().unwrap()),
        format!(
            "{:?}",
            StoreStats::new(&store).snapshot_on("2026-02-01").unwrap()
        ),
        format!("{listed:?}"),
        format!(
            "{:?}",
            listed
                .iter()
                .map(|r| revisions.revision("m1", r.id).unwrap())
                .collect::<Vec<_>>()
        ),
        format!(
            "{:?}",
            WikiIndex::new(&store).recent_first_then_title().unwrap()
        ),
        format!("{:?}", WikiIndex::new(&store).link_count("alpha").unwrap()),
        format!(
            "{:?}",
            WikiIndex::new(&store)
                .search(&["quokkas".to_string()], 10)
                .unwrap()
        ),
        format!("{:?}", WikiIndex::new(&store).meta("compiled_at").unwrap()),
    ]
}

#[test]
fn a_copied_store_reads_back_as_the_source_does() {
    let source = seeded_store();
    let (target, report) = copied_store(&source);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    for (table, n) in [
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
    let ours = read_back_groups(&source);
    let theirs = read_back_groups(&target);
    for (a, b) in ours.iter().zip(&theirs) {
        assert_eq!(b, a);
    }
    assert_eq!(theirs.len(), ours.len());
    // The core arrived too.
    assert_eq!(read_back(&target), read_back(&source));
}

#[test]
fn new_ids_continue_past_the_copied_ones() {
    let source = seeded_store();
    let (target, _) = copied_store(&source);
    let store = target.store();
    let snapshot = StoreStats::new(&store)
        .insert_snapshot(&AnalyticsSnapshot {
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
            &Tracked {
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
    let ids: Vec<i64> = Revisions::new(&store)
        .list("m1", 10)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| *id >= 1));
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "no revision id was reissued: {ids:?}");
}

#[test]
fn the_whole_copy_refuses_a_target_with_any_group_filled() {
    let source = seeded_store();
    let target = Database::open_in_memory_on_engine().unwrap();
    {
        let store = target.store();
        WikiIndex::new(&store).set_meta("k", "v").unwrap();
    }
    let source = source.store();
    let target = target.store();
    let refused = copy_store(
        source.sqlite().unwrap(),
        &mut target.engine().unwrap().lock(),
    );
    assert!(matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("not empty")));
}
