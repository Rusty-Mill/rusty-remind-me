//! The search wall-clock deadline (#257).
//!
//! The reference caps retrieval three ways — item count, character budget and
//! timeout. This port had the first two and no clock at all.
//!
//! # What these tests are careful about
//!
//! The deadline gates **stage entry**, not work already running. A socket read
//! inside the embedder is bounded by that embedder's own `IO_TIMEOUT`, not by
//! this, so the worst case is the deadline plus one in-flight stage. That is a
//! real limit, and `the_deadline_gates_entry_it_does_not_interrupt` pins it
//! down deliberately rather than leaving it as an unstated assumption — if
//! someone later makes the deadline preemptive, that test should be the one
//! that tells them the contract changed.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::retrieval::{Deadline, SEARCH_DEADLINE_ENV};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The deadline is read from a process-global env var; serialize every test
/// that sets it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const DIM: usize = 26;

/// Records whether it was reached at all.
///
/// The interesting assertion is a *negative* one — that a stage was never
/// entered — and that cannot be made by looking at results, since a search
/// with no semantic hits looks the same as one where semantic never ran.
struct CountingEmbedder {
    calls: AtomicUsize,
}

impl CountingEmbedder {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1f32; DIM]).collect())
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "counting".into(),
            model: "test".into(),
            dim: DIM,
        }
    }
}

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_deadline_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: Vec::new(),
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: Vec::new(),
            sensitive: false,
        },
    )
    .unwrap();
}

fn query(q: &str) -> MemorySearchInput {
    MemorySearchInput {
        query: q.to_string(),
        ..Default::default()
    }
}

/// Restores the variable on the way out, so a failing assertion cannot leave
/// every later test in the binary running under a deadline.
struct EnvGuard;
impl EnvGuard {
    fn set(ms: &str) -> Self {
        crate::test_env::set_var(SEARCH_DEADLINE_ENV, ms);
        EnvGuard
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        crate::test_env::remove_var(SEARCH_DEADLINE_ENV);
    }
}

#[test]
fn without_a_deadline_nothing_is_reported_and_nothing_is_skipped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(SEARCH_DEADLINE_ENV);

    let db = db("unset");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    let embedder = CountingEmbedder::new();
    let outcome =
        queries::search_memories_budgeted(&conn, &query("quokka"), Some(&embedder)).unwrap();

    assert!(!outcome.timing.degraded());
    assert!(outcome.timing.skipped.is_empty());
    assert_eq!(outcome.timing.deadline_ms, None);
    assert!(!outcome.results.is_empty());
    // Unset must mean unbounded, not "expire immediately" -- the semantic
    // stage still ran.
    assert_eq!(embedder.calls(), 1);
}

#[test]
fn an_already_expired_deadline_skips_the_semantic_stage() {
    let db = db("expired");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    // Passed in rather than set in the environment. The env clock starts when
    // the search starts, so an env-configured 1ms deadline expires only if the
    // keyword stage happens to take longer than 1ms -- which is a race, and
    // was flaky in exactly that way before this seam existed.
    let embedder = CountingEmbedder::new();
    let outcome = queries::search_memories_deadlined(
        &conn,
        &query("quokka"),
        Some(&embedder),
        Deadline::already_passed(),
    )
    .unwrap();

    assert!(outcome.timing.degraded());
    assert_eq!(outcome.timing.deadline_ms, Some(0));

    // Exact contents, not `.any(...)`. A loose check passed while the rerank
    // gate was reporting a stage this build cannot run: `reranker::enabled()`
    // defaults to true, but `available()` is `cfg!(feature = "rerank")` and is
    // false by default, so the skip list gained a fabricated entry that a
    // substring assertion could not see.
    let expected: Vec<String> = if cfg!(feature = "rerank") {
        vec!["semantic".to_string(), "rerank".to_string()]
    } else {
        vec!["semantic".to_string()]
    };
    assert_eq!(
        outcome.timing.skipped, expected,
        "only stages that would actually have run may be reported as skipped"
    );

    // Degraded, not failed: the keyword half still answered.
    assert!(
        !outcome.results.is_empty(),
        "a deadline must degrade the search, not kill it"
    );
    assert_eq!(
        embedder.calls(),
        0,
        "the embedder must not be reached once the deadline has passed"
    );
}

#[test]
fn the_deadline_gates_entry_it_does_not_interrupt() {
    let db = db("gate_only");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    // Generous enough that it cannot have passed by the semantic stage.
    let embedder = CountingEmbedder::new();
    let outcome = queries::search_memories_deadlined(
        &conn,
        &query("quokka"),
        Some(&embedder),
        Deadline::starting_now(Some(std::time::Duration::from_secs(60))),
    )
    .unwrap();

    // The contract, stated as a test: a deadline that has *not* passed does
    // not stop the stage starting, and once started nothing here can stop it.
    // A slow embedder therefore overruns the deadline by however long it takes
    // to return. Bounding that means deriving the socket timeout from the
    // remaining budget inside the embedder itself.
    assert_eq!(embedder.calls(), 1);
    assert!(!outcome.timing.degraded());
}

#[test]
fn a_zero_or_malformed_deadline_reads_as_unbounded() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Zero would otherwise expire before the first check and silently reduce
    // every search to keyword-only -- a footgun disguised as a valid setting.
    let _env = EnvGuard::set("0");
    assert_eq!(Deadline::from_env().limit_ms(), None);
    assert!(!Deadline::from_env().expired());

    // Malformed falls back rather than failing the search around it.
    let _env = EnvGuard::set("soon");
    assert_eq!(Deadline::from_env().limit_ms(), None);

    let _env = EnvGuard::set("-5");
    assert_eq!(Deadline::from_env().limit_ms(), None);

    let _env = EnvGuard::set("250");
    assert_eq!(Deadline::from_env().limit_ms(), Some(250));
}

#[test]
fn elapsed_time_is_reported_even_on_a_clean_run() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(SEARCH_DEADLINE_ENV);

    let db = db("elapsed");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    let outcome = queries::search_memories_budgeted(&conn, &query("quokka"), None).unwrap();
    // "How long did that take" is a fair question whether or not anything was
    // cut -- the same reasoning that has `trim_by_token_budget` count tokens
    // under an unlimited budget.
    assert!(
        outcome.timing.elapsed_ms < 60_000,
        "sanity, not a benchmark"
    );
    assert!(!outcome.timing.degraded());
}

#[test]
fn an_empty_query_still_reports_its_timing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(SEARCH_DEADLINE_ENV);

    let db = db("empty_query");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    // Punctuation only: sanitizes to an empty FTS expression and takes the
    // early return, which is easy to forget when adding a field.
    let outcome = queries::search_memories_budgeted(&conn, &query("???"), None).unwrap();
    assert!(outcome.results.is_empty());
    assert!(!outcome.timing.degraded());
    assert_eq!(outcome.timing.deadline_ms, None);
}

#[test]
fn a_category_containing_a_single_quote_is_matched_via_bound_parameter() {
    // Regression test for #276: the category filter used to be interpolated
    // into the SQL string with manual `'`-doubling instead of a `?`
    // placeholder. That happened to be safe, but a category value containing
    // a quote is exactly the input that would have broken it -- so it is the
    // input this test exercises, on the code path (`search_memories_budgeted`
    // / `search_memories_deadlined`) that had the bug.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(SEARCH_DEADLINE_ENV);

    let db = db("quote_category");
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "quokka sightings on the island".to_string(),
            category: "foo's bar".to_string(),
            tags: Vec::new(),
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: Vec::new(),
            sensitive: false,
        },
    )
    .unwrap();
    // A memory in a different category must not leak through the filter.
    add(&conn, "quokka but in the wrong category");

    let mut input = query("quokka");
    input.category = Some("foo's bar".to_string());

    let outcome = queries::search_memories_budgeted(&conn, &input, None).unwrap();

    assert_eq!(
        outcome.results.len(),
        1,
        "the category filter should match exactly the memory whose category \
         contains a single quote, not be broken by it"
    );
    assert_eq!(outcome.results[0].memory.category, "foo's bar");
}

#[test]
fn the_configured_deadline_reaches_the_search_response() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::set("2500");

    let db = db("response");
    let conn = db.conn();
    add(&conn, "quokka sightings on the island");

    // The full path an MCP caller takes, not just the inner function: this is
    // the only test that proves the env var is actually consulted on the real
    // entry point and that the timing survives being copied onto the response.
    // It deliberately asserts the deadline was *carried*, not that it expired
    // -- expiry is covered above, deterministically, through the seam.
    let res = queries::search_with_expansions(&conn, &query("quokka")).unwrap();
    assert_eq!(res.timing.deadline_ms, Some(2500));
    assert!(!res.timing.degraded(), "2.5s is not a tight budget");
    assert!(!res.memories.is_empty());
}
