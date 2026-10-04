//! Coverage for the write context, writer provenance and the
//! project/branch/session/writer filters (schema v32, Wave 1B). Engine only.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::context::{ScopeFilter, CWD_ENV, SESSION_ID_ENV, WRITTEN_BY_ENV};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::importer::import_bytes;
use remind_me_core::stats;
use remind_me_core::vitality::{
    feedback_magnitude, record_feedback, FeedbackSignal, FEEDBACK_MAGNITUDE,
};
use remind_me_core::{
    Database, ImportKind, ImportOutcome, MemoryAddInput, MemoryListInput, MemorySearchInput,
};
use std::sync::Mutex;

/// Tests that set process environment variables run one at a time.
static ENV: Mutex<()> = Mutex::new(());

const NOW: &str = "2026-10-04T00:00:00+00:00";

fn add_input(content: &str) -> MemoryAddInput {
    MemoryAddInput {
        content: content.into(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        sensitive: false,
    }
}

/// Insert a row with the given scope columns.
fn put(
    store: &Store<'_>,
    id: &str,
    content: &str,
    project: Option<&str>,
    branch: Option<&str>,
    session: Option<&str>,
    written_by: &str,
) {
    Memories::new(store)
        .insert(&NewMemory {
            project: project.map(str::to_string),
            git_branch: branch.map(str::to_string),
            session_id: session.map(str::to_string),
            written_by: written_by.to_string(),
            ..NewMemory::new(id, content, NOW)
        })
        .unwrap();
}

fn ids(page: &remind_me_core::MemoryListResult) -> Vec<String> {
    let mut ids: Vec<String> = page.memories.iter().map(|m| m.id.clone()).collect();
    ids.sort();
    ids
}

fn list(store: &Store<'_>, scope: ScopeFilter) -> Vec<String> {
    let input = MemoryListInput {
        limit: 100,
        scope,
        ..Default::default()
    };
    ids(&queries::list_memories(store, &input).unwrap())
}

fn search(store: &Store<'_>, scope: ScopeFilter) -> Vec<String> {
    let input = MemorySearchInput {
        query: "quokka".into(),
        scope,
        ..Default::default()
    };
    let mut got: Vec<String> = queries::search_memories_with_embedder(store, &input, None)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect();
    got.sort();
    got
}

fn fixture(store: &Store<'_>) {
    put(
        store,
        "a",
        "quokka alpha",
        Some("Rusty"),
        Some("main"),
        Some("s1"),
        "human",
    );
    put(
        store,
        "b",
        "quokka beta",
        Some("rusty"),
        Some("dev"),
        Some("s2"),
        "hook",
    );
    put(
        store,
        "c",
        "quokka gamma",
        Some("other"),
        Some("main"),
        Some("s1"),
        "model",
    );
    put(
        store,
        "d",
        "quokka delta",
        None,
        None,
        None,
        "importer:chat_import",
    );
}

fn scope(p: Option<&str>, b: Option<&str>, s: Option<&str>, w: Option<&str>) -> ScopeFilter {
    let own = |v: Option<&str>| v.map(str::to_string);
    ScopeFilter::new(own(p), own(b), own(s), own(w))
}

#[test]
fn list_filters_by_each_scope_field() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    fixture(&store);

    assert_eq!(list(&store, ScopeFilter::default()), ["a", "b", "c", "d"]);
    // Project is case-insensitive: "Rusty" and "rusty" both match.
    assert_eq!(
        list(&store, scope(Some("RUSTY"), None, None, None)),
        ["a", "b"]
    );
    // Branch is exact.
    assert_eq!(
        list(&store, scope(None, Some("main"), None, None)),
        ["a", "c"]
    );
    assert_eq!(
        list(&store, scope(None, Some("Main"), None, None)),
        Vec::<String>::new()
    );
    assert_eq!(
        list(&store, scope(None, None, Some("s1"), None)),
        ["a", "c"]
    );
    assert_eq!(list(&store, scope(None, None, None, Some("hook"))), ["b"]);
    // Filters combine.
    assert_eq!(
        list(
            &store,
            scope(Some("rusty"), Some("main"), Some("s1"), Some("human"))
        ),
        ["a"]
    );
    assert!(list(&store, scope(Some("nope"), None, None, None)).is_empty());
}

#[test]
fn list_total_counts_only_the_filtered_set() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    fixture(&store);
    let input = MemoryListInput {
        limit: 1,
        scope: scope(Some("rusty"), None, None, None),
        ..Default::default()
    };
    let page = queries::list_memories(&store, &input).unwrap();
    assert_eq!((page.total, page.count), (2, 1));
}

#[test]
fn search_filters_by_each_scope_field() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    fixture(&store);

    assert_eq!(search(&store, ScopeFilter::default()), ["a", "b", "c", "d"]);
    assert_eq!(
        search(&store, scope(Some("rusty"), None, None, None)),
        ["a", "b"]
    );
    assert_eq!(search(&store, scope(None, Some("dev"), None, None)), ["b"]);
    assert_eq!(
        search(&store, scope(None, None, Some("s1"), None)),
        ["a", "c"]
    );
    assert_eq!(
        search(
            &store,
            scope(None, None, None, Some("importer:chat_import"))
        ),
        ["d"]
    );
    assert!(search(&store, scope(None, Some("zzz"), None, None)).is_empty());
}

#[test]
fn scope_filters_deserialise_from_flat_tool_arguments() {
    let input: MemoryListInput =
        serde_json::from_value(serde_json::json!({"project": "x", "branch": "b", "limit": 5}))
            .unwrap();
    assert_eq!(input.scope.project.as_deref(), Some("x"));
    assert_eq!(input.scope.branch.as_deref(), Some("b"));
    let search: MemorySearchInput = serde_json::from_value(
        serde_json::json!({"query": "q", "session_id": "s", "written_by": "hook"}),
    )
    .unwrap();
    assert_eq!(search.scope.session_id.as_deref(), Some("s"));
    assert_eq!(search.scope.written_by.as_deref(), Some("hook"));
}

#[test]
fn add_memory_stamps_context_and_writer_from_the_environment() {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("rmm-ctx-add-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    test_env::set_var(CWD_ENV, &dir);
    test_env::set_var(SESSION_ID_ENV, "sess-42");
    test_env::remove_var(WRITTEN_BY_ENV);

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let plain = queries::add_memory(&store, add_input("from a model")).unwrap();
    test_env::set_var(WRITTEN_BY_ENV, "hook");
    let hooked = queries::add_memory(&store, add_input("from a hook")).unwrap();
    test_env::set_var(WRITTEN_BY_ENV, "not-a-writer");
    let bogus = queries::add_memory(&store, add_input("bad override")).unwrap();

    test_env::remove_var(CWD_ENV);
    test_env::remove_var(SESSION_ID_ENV);
    test_env::remove_var(WRITTEN_BY_ENV);
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::remove_dir_all(&dir).ok();

    // A server process is MCP: `model`, and a typed note is not auto-captured.
    assert_eq!(plain.written_by, "model");
    assert_eq!(plain.capture_method, "manual");
    assert_eq!(plain.session_id.as_deref(), Some("sess-42"));
    assert_eq!(plain.cwd.as_deref(), Some(dir.to_string_lossy().as_ref()));
    // Outside a repository the project is the directory's name.
    if plain.git_sha.is_none() {
        assert_eq!(plain.project.as_deref(), Some(name.as_str()));
    }
    assert_eq!(hooked.written_by, "hook");
    // An unrecognised override is ignored, not stored.
    assert_eq!(bogus.written_by, "model");
}

#[test]
fn an_importer_stamps_itself_and_keeps_only_the_project() {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(SESSION_ID_ENV, "sess-import");
    test_env::set_var(WRITTEN_BY_ENV, "hook");
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let outcome = import_bytes(
        &store,
        b"# Title\n\nSome durable note about quokkas.\n",
        "note.md",
        "document",
        &[],
        "assistant_messages",
        10_000,
        ImportKind::Document,
    )
    .unwrap();
    test_env::remove_var(SESSION_ID_ENV);
    test_env::remove_var(WRITTEN_BY_ENV);
    assert!(
        matches!(outcome, ImportOutcome::Imported { .. }),
        "{outcome:?}"
    );

    let page = queries::list_memories(
        &store,
        &MemoryListInput {
            limit: 100,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!page.memories.is_empty());
    for m in &page.memories {
        // The environment's `hook` does not override an importer.
        assert_eq!(m.written_by, "importer:document_import");
        assert_eq!(m.capture_method, "auto");
        assert_eq!(m.session_id, None);
        assert_eq!(m.git_sha, None);
    }
}

#[test]
fn feedback_from_machine_written_memories_counts_half() {
    assert_eq!(feedback_magnitude("human"), FEEDBACK_MAGNITUDE);
    assert_eq!(feedback_magnitude("model:opus"), FEEDBACK_MAGNITUDE);
    assert_eq!(feedback_magnitude("hook"), FEEDBACK_MAGNITUDE / 2.0);
    assert_eq!(feedback_magnitude("importer:x"), FEEDBACK_MAGNITUDE / 2.0);

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    put(&store, "h", "x", None, None, None, "human");
    put(&store, "i", "x", None, None, None, "importer:chat_import");
    put(&store, "k", "x", None, None, None, "hook");
    let weight = |id: &str| {
        remind_me_core::testing::memory_f64(&store, id, "base_weight")
            .unwrap()
            .unwrap()
    };
    for id in ["h", "i", "k"] {
        record_feedback(&store, id, FeedbackSignal::Helpful, None).unwrap();
    }
    assert!((weight("h") - (1.0 + FEEDBACK_MAGNITUDE)).abs() < 1e-9);
    assert!((weight("i") - (1.0 + FEEDBACK_MAGNITUDE / 2.0)).abs() < 1e-9);
    assert!((weight("k") - (1.0 + FEEDBACK_MAGNITUDE / 2.0)).abs() < 1e-9);

    // Unhelpful is halved the same way.
    record_feedback(&store, "i", FeedbackSignal::Unhelpful, None).unwrap();
    let expected = (1.0 + FEEDBACK_MAGNITUDE / 2.0) * (1.0 - FEEDBACK_MAGNITUDE / 2.0);
    assert!((weight("i") - expected).abs() < 1e-9);
}

#[test]
fn stats_list_the_ten_busiest_projects() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // project "p00" has 12 memories, "p01" 11, ... "p11" has 1; none has no project.
    for p in 0..12 {
        for n in 0..(12 - p) {
            put(
                &store,
                &format!("m{p}-{n}"),
                "x",
                Some(&format!("p{p:02}")),
                None,
                None,
                "human",
            );
        }
    }
    put(&store, "loose", "x", None, None, None, "human");

    let got = stats::collect(&store).unwrap();
    assert_eq!(got.by_project.len(), 10);
    assert_eq!(got.by_project[0].project, "p00");
    assert_eq!(got.by_project[0].count, 12);
    assert_eq!(got.by_project[9].project, "p09");
    assert!(got.by_project.windows(2).all(|w| w[0].count >= w[1].count));
}

#[test]
fn stats_without_projects_have_an_empty_list() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    put(&store, "a", "x", None, None, None, "human");
    assert!(stats::collect(&store).unwrap().by_project.is_empty());
}

#[test]
fn markdown_header_shows_project_at_branch_and_never_the_session() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    put(
        &store,
        "a",
        "quokka",
        Some("rusty"),
        Some("main"),
        Some("secret-session"),
        "human",
    );
    put(&store, "b", "quokka", Some("rusty"), None, None, "human");
    put(&store, "c", "quokka", None, Some("main"), None, "human");
    let page = queries::list_memories(
        &store,
        &MemoryListInput {
            limit: 100,
            ..Default::default()
        },
    )
    .unwrap();
    let md = remind_me_core::reminders::render_memories_markdown(&page.memories);
    assert!(md.contains("### Memory `a` — rusty@main"), "{md}");
    assert!(md.contains("### Memory `b` — rusty\n"), "{md}");
    assert!(md.contains("### Memory `c`\n"), "{md}");
    assert!(!md.contains("secret-session"));
    // JSON still carries it.
    let json = serde_json::to_string(&page.memories).unwrap();
    assert!(json.contains("secret-session"));
}
