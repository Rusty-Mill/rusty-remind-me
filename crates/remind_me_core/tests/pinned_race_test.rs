//! Pinned questions under concurrent writers (card #61): saving, pinning
//! and answering each read, check and write in one transaction, so neither
//! side of a save racing an answer is lost. Run on both backends.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::sqlite::SqliteTables;
use remind_me_core::db::Store;
use remind_me_core::saved_searches::{
    answer_pinned, answer_pinned_pausing, get_saved_search, pin_saved_search, save_search,
    save_search_pausing, stale_pinned, StaleReason,
};
use remind_me_core::wiki::write_wiki_page;
use remind_me_core::{AnswerPinnedInput, Database, SaveSearchInput};
use std::sync::mpsc;
use std::time::Duration;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_pin_race_{name}_{}",
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

fn save_input(query: &str) -> SaveSearchInput {
    SaveSearchInput {
        name: "deploys".to_string(),
        query: query.to_string(),
        category: None,
        tags: None,
        include_sensitive: false,
        watch: false,
    }
}

fn answer_input() -> AnswerPinnedInput {
    AnswerPinnedInput {
        name: "deploys".to_string(),
        answer_slug: "deploys".to_string(),
        evidence_ids: vec!["mem_a".to_string()],
        as_of: None,
    }
}

/// A pinned "deploys" question with its answer page and one memory.
fn pinned(store: &Store<'_>) {
    let at = chrono::Utc::now().to_rfc3339();
    Memories::new(store)
        .insert(&NewMemory::new("mem_a", "deploy runs on fridays", &at))
        .unwrap();
    save_search(store, &save_input("deploy")).unwrap();
    write_wiki_page(store, "deploys", "Deploys", "Fridays.", "").unwrap();
    pin_saved_search(store, "deploys").unwrap().unwrap();
}

/// Run `first` on a lock-free store, pausing at its read; while it waits,
/// run `second` on the database's own store. Without one transaction around
/// `first`, `second` lands inside its window.
fn race(
    db: &Database,
    first: impl Fn(&Store<'_>, &dyn Fn()) + Send + Sync,
    second: impl FnOnce(&Store<'_>),
) {
    let source = db.secondary_source().unwrap();
    let (read_tx, read_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done_rx = std::sync::Mutex::new(done_rx);
    std::thread::scope(|scope| {
        let racer = scope.spawn(|| {
            let store = source.store();
            let pause = || {
                read_tx.send(()).unwrap();
                let _ = done_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(300));
            };
            first(&store, &pause);
        });
        read_rx.recv().unwrap();
        second(&db.store());
        let _ = done_tx.send(());
        racer.join().unwrap();
    });
}

#[test]
fn an_answer_racing_a_save_does_not_restore_the_old_query() {
    for (_dir, db) in backends("answer_vs_save") {
        pinned(&db.store());
        race(
            &db,
            |store, pause| {
                answer_pinned_pausing(store, &answer_input(), pause).unwrap();
            },
            |store| {
                save_search(store, &save_input("deploy friday")).unwrap();
            },
        );
        let store = db.store();
        let saved = get_saved_search(&store, "deploys").unwrap().unwrap();
        assert_eq!(saved.query, "deploy friday", "the save is kept");
        let pin = saved.filters.pin.unwrap();
        assert_eq!(pin.answered_query.as_deref(), Some("deploy"));
        let reasons: Vec<StaleReason> = stale_pinned(&store)
            .unwrap()
            .into_iter()
            .flat_map(|s| s.reasons)
            .collect();
        assert!(reasons.contains(&StaleReason::QueryChanged), "{reasons:?}");
    }
}

#[test]
fn a_save_racing_an_answer_keeps_the_answer() {
    for (_dir, db) in backends("save_vs_answer") {
        pinned(&db.store());
        race(
            &db,
            |store, pause| {
                save_search_pausing(store, &save_input("deploy friday"), pause).unwrap();
            },
            |store| {
                answer_pinned(store, &answer_input()).unwrap();
            },
        );
        let store = db.store();
        let saved = get_saved_search(&store, "deploys").unwrap().unwrap();
        assert_eq!(saved.query, "deploy friday");
        let pin = saved.filters.pin.expect("the pin is kept");
        assert_eq!(
            pin.answer_slug.as_deref(),
            Some("deploys"),
            "the answer is kept"
        );
        assert_eq!(pin.answered_query.as_deref(), Some("deploy friday"));
        assert!(stale_pinned(&store).unwrap().is_empty());
    }
}
