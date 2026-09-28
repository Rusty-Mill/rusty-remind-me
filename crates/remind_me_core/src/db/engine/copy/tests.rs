//! The copy's tests: a rich SQLite store copied onto the engine must read
//! back through every repository exactly as the source does.

use super::*;
use crate::db::derived::Origin;
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
use parking_lot::Mutex;

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
        let tables: &Mutex<EngineTables> = target.core().unwrap();
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
        format!("{:?}", Memories::new(&store).all_live().unwrap()),
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
