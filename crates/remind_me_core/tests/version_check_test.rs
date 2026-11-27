//! Version checks on writes (card #1487, pack D105-D108): a write that
//! names the version it read lands only if nothing changed since; otherwise
//! nothing is written and the error carries the current version.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries::{self, update_memory_pausing};
use remind_me_core::db::sqlite::SqliteTables;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::history::history;
use remind_me_core::kinds::StructuredFields;
use remind_me_core::version::{self, Expect};
use remind_me_core::wiki_fs::Wiki;
use remind_me_core::{Database, MemoryUpdateInput, UpdateOutcome};
use serde_json::json;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_version_{name}_{}",
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

fn seed(store: &Store<'_>) -> String {
    let at = chrono::Utc::now().to_rfc3339();
    Memories::new(store)
        .insert(&NewMemory::new("mem_v", "deploys run on fridays", &at))
        .unwrap();
    "mem_v".to_string()
}

fn edit(id: &str, content: &str, if_version: Option<&str>) -> MemoryUpdateInput {
    MemoryUpdateInput {
        memory_id: id.to_string(),
        content: Some(content.to_string()),
        category: None,
        tags: None,
        metadata: None,
        sensitive: None,
        clear_superseded: false,
        project: None,
        if_version: if_version.map(str::to_string),
    }
}

fn current(store: &Store<'_>, id: &str) -> remind_me_core::Memory {
    queries::get_memory_by_id(store, id).unwrap().unwrap()
}

fn conflict_version<T: std::fmt::Debug>(result: Result<T, StoreError>) -> String {
    match result {
        Err(StoreError::Conflict { current_version }) => current_version,
        other => panic!("expected a version conflict, got {other:?}"),
    }
}

#[test]
fn a_matching_version_writes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = seed(&store);
    let read = version::of_memory(&current(&store, &id));
    let outcome =
        queries::update_memory(&store, &edit(&id, "deploys run on thursdays", Some(&read)));
    assert!(
        matches!(outcome, Ok(UpdateOutcome::Updated(_))),
        "{outcome:?}"
    );
    let now = current(&store, &id);
    assert_eq!(now.content, "deploys run on thursdays");
    assert_ne!(
        version::of_memory(&now),
        read,
        "the version moved with the edit"
    );
}

#[test]
fn a_stale_version_writes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = seed(&store);
    let read = version::of_memory(&current(&store, &id));
    queries::update_memory(&store, &edit(&id, "first writer", None)).unwrap();
    let before = current(&store, &id);
    let revisions = history(&store, &id, 100).unwrap().len();

    let refused = queries::update_memory(&store, &edit(&id, "second writer", Some(&read)));

    assert_eq!(conflict_version(refused), version::of_memory(&before));
    let after = current(&store, &id);
    assert_eq!(after.content, "first writer");
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(
        history(&store, &id, 100).unwrap().len(),
        revisions,
        "no revision"
    );
}

#[test]
fn no_precondition_is_last_write_wins() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = seed(&store);
    queries::update_memory(&store, &edit(&id, "one", None)).unwrap();
    queries::update_memory(&store, &edit(&id, "two", None)).unwrap();
    assert_eq!(current(&store, &id).content, "two");
}

#[test]
fn a_malformed_version_is_refused_before_reading() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = seed(&store);
    for bad in ["2026-11-27T00:00:00Z", "absent", "v1:short"] {
        let refused = queries::update_memory(&store, &edit(&id, "x", Some(bad)));
        assert!(
            matches!(refused, Err(StoreError::Invalid(_))),
            "{bad}: {refused:?}"
        );
    }
    let missing = queries::update_memory(&store, &edit("mem_none", "x", Some("v1:zz")));
    assert!(matches!(missing, Err(StoreError::Invalid(_))));
    assert_eq!(current(&store, &id).content, "deploys run on fridays");
}

/// Two writers hold the same version. The first pauses between its read and
/// its write while the second runs; exactly one of them lands.
#[test]
fn of_two_writers_with_one_version_exactly_one_wins() {
    for (_dir, db) in backends("two_writers") {
        let id = seed(&db.store());
        let read = version::of_memory(&current(&db.store(), &id));
        let source = db.secondary_source().unwrap();
        let first_result: Mutex<Option<Result<UpdateOutcome, StoreError>>> = Mutex::new(None);
        let (read_tx, read_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let done_rx = Mutex::new(done_rx);
        let second_result = std::thread::scope(|scope| {
            let racer = scope.spawn(|| {
                let store = source.store();
                let pause = || {
                    let _ = read_tx.send(());
                    let _ = done_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_millis(300));
                };
                let outcome = update_memory_pausing(
                    &store,
                    &edit(&id, "first", Some(&read)),
                    &StructuredFields::default(),
                    &pause,
                );
                *first_result.lock().unwrap() = Some(outcome);
            });
            read_rx.recv().unwrap();
            let second = queries::update_memory(&db.store(), &edit(&id, "second", Some(&read)));
            let _ = done_tx.send(());
            racer.join().unwrap();
            second
        });
        let first = first_result.into_inner().unwrap().unwrap();
        let won: Vec<&str> = [("first", first.is_ok()), ("second", second_result.is_ok())]
            .iter()
            .filter(|(_, ok)| *ok)
            .map(|(who, _)| *who)
            .collect();
        assert_eq!(won.len(), 1, "first {first:?}, second {second_result:?}");
        let loser = if first.is_ok() { second_result } else { first };
        conflict_version(loser);
        assert_eq!(current(&db.store(), &id).content, won[0]);
    }
}

#[test]
fn a_wiki_write_checks_the_page_body() {
    let dir = Scratch::new("wiki");
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let wiki = Wiki::new(dir.0.join("wiki"));

    let created = wiki
        .write_page_if(&store, "Deploys", "Fridays.", None, Some(&Expect::Absent))
        .unwrap()
        .unwrap();
    assert!(created.created);
    let page = wiki.read_page(&store, "deploys").unwrap().unwrap();
    assert_eq!(created.version, version::of_text(&page.content));

    let again = wiki.write_page_if(&store, "Deploys", "Mondays.", None, Some(&Expect::Absent));
    assert_eq!(
        conflict_version(again),
        created.version,
        "absent: create only"
    );

    let stale = Expect::Version(version::of_text("something else"));
    let refused = wiki.write_page_if(&store, "Deploys", "Mondays.", None, Some(&stale));
    assert_eq!(conflict_version(refused), created.version);
    let unchanged = wiki.read_page(&store, "deploys").unwrap().unwrap();
    assert_eq!(unchanged.content, page.content, "nothing written");

    let fresh = Expect::Version(created.version.clone());
    let written = wiki
        .write_page_if(&store, "Deploys", "Thursdays.", None, Some(&fresh))
        .unwrap()
        .unwrap();
    assert!(!written.created);
    assert_ne!(written.version, created.version);

    let missing = Expect::Version(created.version);
    let gone = wiki.write_page_if(&store, "Other", "x", None, Some(&missing));
    assert_eq!(conflict_version(gone), version::ABSENT);
    assert!(wiki.read_page(&store, "other").unwrap().is_none());
}

#[test]
fn a_project_write_checks_its_claim_chain() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let call = |args: serde_json::Value| remind_me_core::project_tool::call(&store, &args);

    let unknown = call(json!({ "action": "get", "project": "alpha" })).unwrap();
    let empty = unknown["version"].as_str().unwrap().to_string();
    let set = call(json!({
        "action": "set", "project": "alpha", "stage": "idea", "status": "active",
        "because": "started", "if_version": empty,
    }))
    .unwrap();
    let after_set = set["version"].as_str().unwrap().to_string();
    assert_ne!(after_set, empty);
    let got = call(json!({ "action": "get", "project": "alpha" })).unwrap();
    assert_eq!(got["version"], after_set.as_str());

    let refused = call(json!({
        "action": "set", "project": "alpha", "stage": "designed",
        "because": "drawn up", "if_version": empty,
    }));
    assert_eq!(conflict_version(refused), after_set);
    let unchanged = call(json!({ "action": "get", "project": "alpha" })).unwrap();
    assert_eq!(unchanged["version"], after_set.as_str(), "nothing written");

    let bad = call(json!({
        "action": "set", "project": "alpha", "stage": "designed",
        "because": "drawn up", "if_version": "absent",
    }));
    assert!(matches!(bad, Err(StoreError::Invalid(_))), "{bad:?}");

    call(json!({
        "action": "set", "project": "alpha", "stage": "designed",
        "because": "drawn up", "if_version": after_set,
    }))
    .unwrap();
}
