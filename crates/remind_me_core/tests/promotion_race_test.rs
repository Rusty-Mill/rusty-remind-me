//! Concurrent promotions (card #60): a promotion's checks (live sources,
//! the refresh target, duplicate evidence) and its writes are one
//! transaction, so two racing promotions cannot both pass the checks and
//! leave two live artifacts. Run on both backends.

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::sqlite::SqliteTables;
use remind_me_core::db::Store;
use remind_me_core::promotion::{load_memories, promote, promote_pausing, PromotionError};
use remind_me_core::{Database, PromoteInput, Rung, FACT_CATEGORY};
use std::sync::mpsc;
use std::time::Duration;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_promote_race_{name}_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// File-backed databases on each backend, so a second thread can open a
/// store that holds no process lock.
fn backends(name: &str) -> Vec<(Scratch, Database)> {
    let engine = Scratch::new(&format!("{name}_engine"));
    let engine_db = Database::open(engine.0.join("memory.db")).unwrap();
    let sqlite = Scratch::new(&format!("{name}_sqlite"));
    let sqlite_db = Database::open_backend(
        Box::new(SqliteTables::open(&sqlite.0.join("store")).unwrap()),
        Some(sqlite.0.join("memory.db")),
    )
    .unwrap();
    vec![(engine, engine_db), (sqlite, sqlite_db)]
}

/// Three live facts, `f0`..`f2`.
fn facts(store: &Store<'_>) -> Vec<String> {
    let at = chrono::Utc::now().to_rfc3339();
    (0..3)
        .map(|i| {
            let id = format!("f{i}");
            Memories::new(store)
                .insert(&NewMemory {
                    category: FACT_CATEGORY.to_string(),
                    ..NewMemory::new(&id, format!("fact {i}"), &at)
                })
                .unwrap();
            id
        })
        .collect()
}

fn scenario(source_ids: &[String], content: &str, refreshes: Option<&str>) -> PromoteInput {
    PromoteInput {
        rung: Rung::FactToScenario,
        source_ids: source_ids.to_vec(),
        content: content.to_string(),
        refreshes: refreshes.map(str::to_string),
    }
}

/// Run `first` on a lock-free store, pausing once its checks pass; while
/// it waits, run `second` on the database's own store. Without one
/// transaction around `first`, `second` lands inside its window.
fn race<A, B>(
    db: &Database,
    first: impl Fn(&Store<'_>, &dyn Fn()) -> A + Send + Sync,
    second: impl FnOnce(&Store<'_>) -> B,
) -> (A, B)
where
    A: Send,
{
    let source = db.secondary_source().unwrap();
    let (checked_tx, checked_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done_rx = std::sync::Mutex::new(done_rx);
    std::thread::scope(|scope| {
        let racer = scope.spawn(|| {
            let store = source.store();
            let pause = || {
                checked_tx.send(()).unwrap();
                let _ = done_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(300));
            };
            first(&store, &pause)
        });
        checked_rx.recv().unwrap();
        let second_result = second(&db.store());
        let _ = done_tx.send(());
        (racer.join().unwrap(), second_result)
    })
}

/// Exactly one of two racing results succeeded; its promoted id and the
/// other's error.
fn one_winner(
    a: Result<remind_me_core::PromotionResult, PromotionError>,
    b: Result<remind_me_core::PromotionResult, PromotionError>,
) -> (String, PromotionError) {
    match (a, b) {
        (Ok(won), Err(lost)) | (Err(lost), Ok(won)) => (won.promoted_id, lost),
        (a, b) => panic!("expected one success and one refusal, got {a:?} and {b:?}"),
    }
}

#[test]
fn two_refreshes_of_one_artifact_leave_one_live_replacement() {
    for (_dir, db) in backends("refresh") {
        let (ids, old) = {
            let store = db.store();
            let ids = facts(&store);
            let old = promote(&store, &scenario(&ids[..2], "old", None))
                .unwrap()
                .promoted_id;
            (ids, old)
        };
        let refresh = scenario(&ids, "refreshed", Some(&old));
        let (a, b) = race(
            &db,
            |store, pause| promote_pausing(store, &refresh, pause),
            |store| promote(store, &refresh),
        );
        let (won, lost) = one_winner(a, b);
        assert!(
            matches!(&lost, PromotionError::UnusableRefresh(id) if *id == old),
            "{lost:?}"
        );
        let rows = load_memories(&db.store(), std::slice::from_ref(&old)).unwrap();
        assert_eq!(rows[0].superseded_by.as_deref(), Some(won.as_str()));
    }
}

#[test]
fn two_promotions_of_the_same_evidence_leave_one_artifact() {
    for (_dir, db) in backends("duplicate") {
        let ids = facts(&db.store());
        let input = scenario(&ids, "the same pattern", None);
        let (a, b) = race(
            &db,
            |store, pause| promote_pausing(store, &input, pause),
            |store| promote(store, &input),
        );
        let (won, lost) = one_winner(a, b);
        assert!(
            matches!(&lost, PromotionError::DuplicateSources(id) if *id == won),
            "{lost:?}"
        );
    }
}
