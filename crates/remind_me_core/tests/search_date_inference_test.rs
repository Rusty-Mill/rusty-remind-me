//! `REMIND_ME_DATE_PHRASES=on`: a date phrase in the query becomes the search
//! window at the tool's boundary only (card #63). In its own binary: it writes
//! the process env.

#[path = "../src/test_env.rs"]
mod test_env;

use chrono::{Duration, Utc};
use chrono_tz::Tz;
use remind_me_core::date_inference::FLAG_ENV;
use remind_me_core::date_phrase::{self, TZ_ENV};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::models::MemorySearchInput;
use remind_me_core::Database;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets the env for one test and restores what was there.
struct Env(Vec<(&'static str, Option<String>)>);

impl Env {
    fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let mut old = Vec::new();
        for (key, value) in vars {
            old.push((*key, std::env::var(key).ok()));
            match value {
                Some(v) => test_env::set_var(key, v),
                None => test_env::remove_var(key),
            }
        }
        Self(old)
    }

    /// Inference on or off, in UTC, with every model stage off.
    fn flag(on: bool) -> Self {
        Self::set(&[
            (FLAG_ENV, on.then_some("on")),
            (TZ_ENV, None),
            (remind_me_core::embedder::EMBEDDING_BACKEND_ENV, None),
            (
                remind_me_core::query_expansion::EXPANSION_MODE_ENV,
                Some("off"),
            ),
            (remind_me_core::reranker::BACKEND_ENV, Some("")),
        ])
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            match value {
                Some(v) => test_env::set_var(key, v),
                None => test_env::remove_var(key),
            }
        }
    }
}

/// Mid-way through last week, and 40 days back: the same words in both.
fn store_with_two_deploy_decisions(db: &Database) -> Store<'_> {
    let store = db.store();
    let now = Utc::now();
    let last_week = date_phrase::parse("last week", now.with_timezone(&Tz::UTC)).unwrap();
    let inside = last_week.start + Duration::days(3);
    let repo = Memories::new(&store);
    for (id, content, at) in [
        (
            "m-new",
            "Deploy decision: dock firmware ships blue-green.",
            inside,
        ),
        (
            "m-old",
            "Deploy decision: dock firmware ships in place.",
            now - Duration::days(40),
        ),
        ("m-other", "Billing moved to the new ledger.", inside),
    ] {
        repo.insert(&NewMemory {
            accessed_at: Some(now.to_rfc3339()),
            ..NewMemory::new(id, content, &at.to_rfc3339())
        })
        .unwrap();
    }
    store
}

fn search(text: &str) -> MemorySearchInput {
    MemorySearchInput {
        query: text.into(),
        ..Default::default()
    }
}

fn ids(
    results: impl IntoIterator<Item = remind_me_core::models::MemorySearchResult>,
) -> Vec<String> {
    let mut ids: Vec<String> = results.into_iter().map(|r| r.memory.id).collect();
    ids.sort();
    ids
}

#[test]
fn on_a_phrase_narrows_the_window_and_leaves_the_query() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = Env::flag(true);
    let db = Database::open_in_memory().unwrap();
    let store = store_with_two_deploy_decisions(&db);

    let res =
        queries::search_with_expansions(&store, &search("deploy decision last week")).unwrap();
    assert_eq!(ids(res.memories), ["m-new"]);
    let applied = res.inferred_window.expect("window reported");
    assert_eq!(applied.phrase, "last week");
    assert_eq!(applied.zone, "UTC");
    assert!(applied.until.is_some());

    // Nothing but fillers left: the window is listed.
    let res = queries::search_with_expansions(&store, &search("what happened last week?")).unwrap();
    assert_eq!(ids(res.memories), ["m-new", "m-other"]);
}

#[test]
fn an_explicit_window_wins_and_off_changes_nothing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = Database::open_in_memory().unwrap();
    let store = store_with_two_deploy_decisions(&db);
    {
        let _env = Env::flag(true);
        let mut explicit = search("deploy decision last week");
        explicit.window = serde_json::from_value(serde_json::json!({ "since": "400d" })).unwrap();
        let res = queries::search_with_expansions(&store, &explicit).unwrap();
        assert!(res.inferred_window.is_none());
        assert_eq!(ids(res.memories), ["m-new", "m-old"]);
    }
    let _env = Env::flag(false);
    let res =
        queries::search_with_expansions(&store, &search("deploy decision last week")).unwrap();
    assert!(res.inferred_window.is_none());
    assert_eq!(ids(res.memories), ["m-new", "m-old"]);
}

#[test]
fn the_ranker_under_saved_searches_and_the_brief_never_infers() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = Env::flag(true);
    let db = Database::open_in_memory().unwrap();
    let store = store_with_two_deploy_decisions(&db);
    let hits = queries::search_memories(&store, &search("deploy decision last week")).unwrap();
    assert_eq!(ids(hits), ["m-new", "m-old"]);
}

#[test]
fn a_bad_zone_fails_only_a_query_with_a_phrase() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = Env::flag(true);
    let _zone = Env::set(&[(TZ_ENV, Some("Mars/Olympus"))]);
    let db = Database::open_in_memory().unwrap();
    let store = store_with_two_deploy_decisions(&db);
    let err = queries::search_with_expansions(&store, &search("deploys yesterday")).unwrap_err();
    assert!(err.to_string().contains("Mars/Olympus"), "{err}");
    assert!(queries::search_with_expansions(&store, &search("deploy decision")).is_ok());
}

#[test]
fn on_uses_the_configured_calendar() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = Env::flag(true);
    let _zone = Env::set(&[(TZ_ENV, Some("Pacific/Kiritimati"))]); // UTC+14
    let db = Database::open_in_memory().unwrap();
    let store = store_with_two_deploy_decisions(&db);
    let res = queries::search_with_expansions(&store, &search("deploy decision today")).unwrap();
    let applied = res.inferred_window.expect("window reported");
    assert_eq!(applied.zone, "Pacific/Kiritimati");
    let kiritimati: Tz = "Pacific/Kiritimati".parse().unwrap();
    let local_midnight = Utc::now()
        .with_timezone(&kiritimati)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(kiritimati)
        .unwrap();
    assert_eq!(applied.since, local_midnight.with_timezone(&Utc));
}
