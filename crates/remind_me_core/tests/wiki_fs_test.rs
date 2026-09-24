//! Coverage for the on-disk wiki: reconcile, load and compile.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::wiki::WikiDeleteOutcome;
use remind_me_core::wiki_fs::{
    extract_summary, extract_title, get_meta, parse_wikilinks, pending_compile_count, Wiki,
    WikiCompile, COMPILE_WATERMARK_KEY, INDEX_FILE, LOG_FILE, SCHEMA_FILE, WIKI_DIR_ENV,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::sync::Mutex;

/// `WIKI_DIR_ENV` is process-global; only one test in this file touches it
/// ([`from_env_defaults_to_the_hyphenated_data_directory`]), but a future
/// one that does would silently race it without this guard -- the same
/// convention `sync_status_test.rs`'s own `ENV_LOCK` documents.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A wiki rooted in its own scratch directory, so tests never share state.
fn wiki(name: &str) -> (Wiki, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("rrm_wiki_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    (Wiki::new(&root), root)
}

fn write_file(root: &std::path::Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();
}

// --- parsing helpers ---------------------------------------------------------

#[test]
fn the_title_is_the_first_h1_or_the_humanised_slug() {
    assert_eq!(
        extract_title("# Real Title\n\nbody", "fallback"),
        "Real Title"
    );
    assert_eq!(extract_title("no heading here", "vlan-setup"), "Vlan Setup");
}

#[test]
fn the_summary_is_the_first_body_line() {
    assert_eq!(
        extract_summary("# Title\n\n- A bullet summary\n\nmore"),
        "A bullet summary"
    );
    assert_eq!(extract_summary("# Only a heading"), "");
}

#[test]
fn wikilinks_resolve_by_target_and_display_by_alias() {
    let links = parse_wikilinks("see [[VLAN Setup]] and [[Networking|the network page]]");
    assert_eq!(
        links,
        vec![
            ("vlan-setup".to_string(), "VLAN Setup".to_string()),
            ("networking".to_string(), "Networking".to_string()),
        ]
    );
}

// --- reconcile ---------------------------------------------------------------

#[test]
fn reconcile_indexes_files_on_disk() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("index");
    write_file(&root, "vlan-setup.md", "# VLAN Setup\n\nTag port 3.");

    let stats = w.reconcile(&conn).unwrap();

    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.pages, 1);
    let page = w.read_page(&conn, "VLAN Setup").unwrap().unwrap();
    assert_eq!(page.title, "VLAN Setup");
    assert_eq!(page.summary, "Tag port 3.");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn reconcile_is_a_no_op_when_nothing_changed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("noop");
    write_file(&root, "page.md", "# Page\n\nbody");
    w.reconcile(&conn).unwrap();

    let second = w.reconcile(&conn).unwrap();

    // Every read path reconciles, so a no-op pass has to actually be cheap.
    assert_eq!(second.indexed, 0);
    assert_eq!(second.removed, 0);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_edit_made_outside_the_server_is_picked_up() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("edited");
    write_file(&root, "page.md", "# Page\n\noriginal");
    w.reconcile(&conn).unwrap();

    // Someone edits the file in their editor. Files are the source of truth,
    // so the index has to follow without being told.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_file(&root, "page.md", "# Page\n\nrevised");
    let stats = w.reconcile(&conn).unwrap();

    assert_eq!(stats.indexed, 1);
    assert!(w
        .read_page(&conn, "page")
        .unwrap()
        .unwrap()
        .content
        .contains("revised"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_file_deleted_outside_the_server_drops_out_of_the_index() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("gone");
    write_file(&root, "page.md", "# Page\n\nbody");
    w.reconcile(&conn).unwrap();

    std::fs::remove_file(root.join("page.md")).unwrap();
    let stats = w.reconcile(&conn).unwrap();

    assert_eq!(stats.removed, 1);
    assert!(w.read_page(&conn, "page").unwrap().is_none());

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn generated_pages_are_not_content_pages() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("generated");
    write_file(&root, INDEX_FILE, "# Wiki Index\n\ngenerated");
    write_file(&root, LOG_FILE, "# Wiki Change Log\n");
    write_file(&root, SCHEMA_FILE, "# Schema\n");
    write_file(&root, "real.md", "# Real\n\nbody");

    let stats = w.reconcile(&conn).unwrap();

    assert_eq!(stats.pages, 1, "only the real page counts");
    assert_eq!(w.list_pages(&conn).unwrap().len(), 1);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn removing_a_link_from_a_page_removes_the_edge() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("links");
    write_file(&root, "page.md", "# Page\n\nsee [[Other]]");
    w.reconcile(&conn).unwrap();
    let edges = |conn: &Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM wiki_links", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(edges(&conn), 1);

    std::thread::sleep(std::time::Duration::from_millis(10));
    write_file(&root, "page.md", "# Page\n\nno links now");
    w.reconcile(&conn).unwrap();

    // Links are replaced wholesale; an insert-only pass would leave the stale
    // edge behind.
    assert_eq!(edges(&conn), 0);

    std::fs::remove_dir_all(&root).unwrap();
}

// --- write / delete ----------------------------------------------------------

#[test]
fn writing_a_page_creates_a_file_and_indexes_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("write");

    let outcome = w
        .write_page(&conn, "VLAN Setup", "Tag port 3 for the lab.", None)
        .unwrap()
        .unwrap();

    assert!(outcome.created);
    assert_eq!(outcome.slug, "vlan-setup");
    let on_disk = std::fs::read_to_string(root.join("vlan-setup.md")).unwrap();
    // Self-describing: someone opening the file in an editor sees what it is.
    assert!(on_disk.starts_with("# VLAN Setup\n"));
    assert_eq!(
        w.read_page(&conn, "vlan-setup").unwrap().unwrap().title,
        "VLAN Setup"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rewriting_a_page_reports_it_as_an_update() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("update");
    w.write_page(&conn, "Page", "first", None).unwrap().unwrap();

    let second = w
        .write_page(&conn, "Page", "second", None)
        .unwrap()
        .unwrap();

    assert!(!second.created);
    assert!(w
        .read_page(&conn, "page")
        .unwrap()
        .unwrap()
        .content
        .contains("second"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_disagreeing_h1_is_replaced_not_duplicated() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("heading");

    w.write_page(&conn, "Correct Title", "# Wrong Title\n\nbody", None)
        .unwrap()
        .unwrap();

    let content = std::fs::read_to_string(root.join("correct-title.md")).unwrap();
    // The file's title has to match the slug it is stored under, or the index
    // lies about what the page is.
    assert!(content.starts_with("# Correct Title\n"));
    assert!(!content.contains("Wrong Title"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn writing_refuses_a_reserved_page() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("reserved");

    let outcome = w.write_page(&conn, "index", "hand-written", None).unwrap();

    assert_eq!(outcome.unwrap_err(), WikiDeleteOutcome::Reserved);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_index_is_regenerated_on_every_write() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("regen");

    w.write_page(&conn, "Alpha", "first page", None)
        .unwrap()
        .unwrap();
    w.write_page(&conn, "Beta", "second page", None)
        .unwrap()
        .unwrap();

    let index = std::fs::read_to_string(root.join(INDEX_FILE)).unwrap();
    assert!(index.contains("[[Alpha]]"));
    assert!(index.contains("[[Beta]]"));
    assert!(index.contains("2 page(s)"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_log_records_writes_and_deletes() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("log");
    w.write_page(&conn, "Page", "body", None).unwrap().unwrap();
    w.delete_page(&conn, "Page").unwrap();

    let log = std::fs::read_to_string(root.join(LOG_FILE)).unwrap();
    assert!(log.contains("created [[Page]]"));
    assert!(log.contains("deleted [[Page]]"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_removes_the_file_as_well_as_the_row() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("delete");
    w.write_page(&conn, "Page", "body", None).unwrap().unwrap();

    assert_eq!(
        w.delete_page(&conn, "Page").unwrap(),
        WikiDeleteOutcome::Deleted
    );

    // A row-only delete would be undone by the next reconcile, since the file
    // is still there.
    assert!(!root.join("page.md").exists());
    assert!(w.read_page(&conn, "page").unwrap().is_none());

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn deleting_resolves_by_title_or_slug_and_refuses_reserved_pages() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("resolve");
    w.write_page(&conn, "VLAN Setup!", "body", None)
        .unwrap()
        .unwrap();

    assert_eq!(
        w.delete_page(&conn, "vlan-setup").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert_eq!(
        w.delete_page(&conn, "missing").unwrap(),
        WikiDeleteOutcome::NotFound
    );
    assert_eq!(
        w.delete_page(&conn, "index").unwrap(),
        WikiDeleteOutcome::Reserved
    );

    std::fs::remove_dir_all(&root).unwrap();
}

// --- load --------------------------------------------------------------------

#[test]
fn loading_an_empty_wiki_returns_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("empty");

    let loaded = w.load(&conn, 0, true).unwrap();

    assert_eq!(loaded.pages_included, 0);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn loading_concatenates_every_page_with_an_index() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("load");
    w.write_page(&conn, "Alpha", "the alpha body", None)
        .unwrap()
        .unwrap();
    w.write_page(&conn, "Beta", "the beta body", None)
        .unwrap()
        .unwrap();

    let loaded = w.load(&conn, 0, true).unwrap();

    assert_eq!(loaded.pages_included, 2);
    assert_eq!(loaded.pages_omitted, 0);
    assert!(loaded.content.contains("# Wiki Index"));
    assert!(loaded.content.contains("the alpha body"));
    assert!(loaded.content.contains("the beta body"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_index_can_be_left_out() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("noindex");
    w.write_page(&conn, "Alpha", "body", None).unwrap().unwrap();

    let loaded = w.load(&conn, 0, false).unwrap();

    assert!(!loaded.content.contains("# Wiki Index"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn overflow_is_listed_by_title_rather_than_silently_dropped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("budget");
    w.write_page(&conn, "Alpha", &"a".repeat(400), None)
        .unwrap()
        .unwrap();
    w.write_page(&conn, "Beta", &"b".repeat(400), None)
        .unwrap()
        .unwrap();
    w.write_page(&conn, "Gamma", &"c".repeat(400), None)
        .unwrap()
        .unwrap();

    let loaded = w.load(&conn, 120, false).unwrap();

    assert!(loaded.pages_omitted > 0);
    // A caller has to be able to tell it got part of the wiki, and what to
    // fetch individually.
    assert!(loaded.content.contains("Omitted (token budget)"));
    assert!(loaded.content.contains("remind_me_wiki_read"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn one_page_always_comes_back_even_when_it_busts_the_budget() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("oversize");
    w.write_page(&conn, "Huge", &"x".repeat(4_000), None)
        .unwrap()
        .unwrap();

    let loaded = w.load(&conn, 10, false).unwrap();

    // Returning an index and nothing else would be useless.
    assert_eq!(loaded.pages_included, 1);

    std::fs::remove_dir_all(&root).unwrap();
}

// --- compile -----------------------------------------------------------------

#[test]
fn compiling_an_empty_store_reports_nothing_pending() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("nothing");

    match w.compile(&conn, 20, false).unwrap() {
        WikiCompile::Noop { .. } => {}
        other => panic!("expected a no-op, got {:?}", other),
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_brief_surfaces_pending_sources_and_the_schema() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("brief");
    add(&conn, "a raw memory about quokkas");

    match w.compile(&conn, 20, false).unwrap() {
        WikiCompile::Brief { pending, brief, .. } => {
            assert_eq!(pending, 1);
            assert!(brief.contains("quokkas"));
            assert!(brief.contains("Maintainer schema"));
            assert!(brief.contains("bootstrapping"));
        }
        other => panic!("expected a brief, got {:?}", other),
    }
    // The schema file is written on first use so it can be edited.
    assert!(root.join(SCHEMA_FILE).exists());

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_brief_never_advances_the_watermark() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("idempotent");
    add(&conn, "a raw memory");

    for _ in 0..3 {
        match w.compile(&conn, 20, false).unwrap() {
            WikiCompile::Brief { pending, .. } => assert_eq!(pending, 1),
            other => panic!("expected a brief, got {:?}", other),
        }
    }

    // Phase one being idempotent is what makes it safe to re-read.
    assert_eq!(get_meta(&conn, COMPILE_WATERMARK_KEY).unwrap(), None);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn marking_integrated_advances_the_watermark_past_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("mark");
    add(&conn, "first");
    add(&conn, "second");

    let marked = w.compile(&conn, 20, true).unwrap();

    match marked {
        WikiCompile::Integrated {
            sources_marked,
            watermark,
        } => {
            assert_eq!(sources_marked, 2);
            assert!(!watermark.is_empty());
        }
        other => panic!("expected an integration, got {:?}", other),
    }
    match w.compile(&conn, 20, false).unwrap() {
        WikiCompile::Noop { .. } => {}
        other => panic!("expected nothing pending, got {:?}", other),
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_memory_written_during_synthesis_is_not_skipped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("during");
    add(&conn, "surfaced in the brief");
    // Only the first is surfaced.
    let brief = w.compile(&conn, 1, false).unwrap();
    assert!(matches!(brief, WikiCompile::Brief { pending: 1, .. }));

    // Something is written while the caller synthesises.
    add(&conn, "written during synthesis");
    w.compile(&conn, 1, true).unwrap();

    // The watermark is the last *surfaced* row's created_at, not the wall
    // clock — a clock-based watermark would swallow this memory unseen.
    match w.compile(&conn, 20, false).unwrap() {
        WikiCompile::Brief { pending, brief, .. } => {
            assert_eq!(pending, 1);
            assert!(brief.contains("written during synthesis"));
        }
        other => panic!(
            "expected the later memory to still be pending, got {:?}",
            other
        ),
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn marking_with_nothing_pending_is_a_reported_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("emptymark");

    match w.compile(&conn, 20, true).unwrap() {
        WikiCompile::Noop { reason, .. } => assert!(reason.contains("no pending")),
        other => panic!("expected a no-op, got {:?}", other),
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_brief_lists_pages_that_already_exist() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("existing");
    w.write_page(&conn, "Networking", "existing knowledge", None)
        .unwrap()
        .unwrap();
    add(&conn, "a new raw memory");

    match w.compile(&conn, 20, false).unwrap() {
        WikiCompile::Brief { brief, .. } => {
            assert!(brief.contains("[[Networking]]"));
            assert!(!brief.contains("bootstrapping"));
        }
        other => panic!("expected a brief, got {:?}", other),
    }

    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// pending_compile_count
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_store_has_nothing_pending() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    assert_eq!(pending_compile_count(&conn).unwrap(), 0);
}

#[test]
fn pending_count_is_not_capped_by_a_brief_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("uncapped");
    for i in 0..5 {
        add(&conn, &format!("memory {}", i));
    }
    // A reconcile (triggered by any Wiki call) does not itself advance the
    // watermark — only compile(mark_integrated: true) does.
    w.list_pages(&conn).unwrap();

    // compile() truncates its own `pending` count to the brief's limit...
    match w.compile(&conn, 2, false).unwrap() {
        WikiCompile::Brief { pending, .. } => assert_eq!(pending, 2),
        other => panic!("expected a brief, got {:?}", other),
    }
    // ...but the true count, which the status route needs, is not.
    assert_eq!(pending_compile_count(&conn).unwrap(), 5);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn marking_integrated_moves_the_watermark_and_drops_the_pending_count() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let (w, root) = wiki("watermark");
    add(&conn, "a raw memory");
    assert_eq!(pending_compile_count(&conn).unwrap(), 1);

    w.compile(&conn, 20, true).unwrap();

    assert_eq!(pending_compile_count(&conn).unwrap(), 0);

    // A memory written after the watermark is pending again.
    add(&conn, "a later memory");
    assert_eq!(pending_compile_count(&conn).unwrap(), 1);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn from_env_defaults_to_the_hyphenated_data_directory() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Regression: this used to hardcode `.remind_me` (underscored) when
    // `REMIND_ME_WIKI_DIR` was unset -- a directory nothing else in this
    // port reads or writes from. See `remote::default_token_file`'s doc
    // for the full story behind the fix applied here.
    //
    // Not steered via a `HOME` env var override: the default now resolves
    // through `import_paths::home_dir_var` (`dirs::home_dir()`), which on
    // Windows reads `%USERPROFILE%` and ignores `HOME` entirely -- the
    // whole point of the fix this test guards. Asserting against that same
    // function's real return value is what stays portable and still pins
    // "hyphenated, not underscored" without depending on which OS this
    // runs on.
    let original_wiki_dir = std::env::var(WIKI_DIR_ENV).ok();
    crate::test_env::remove_var(WIKI_DIR_ENV);

    let wiki = Wiki::from_env();

    match original_wiki_dir {
        Some(v) => crate::test_env::set_var(WIKI_DIR_ENV, v),
        None => crate::test_env::remove_var(WIKI_DIR_ENV),
    }

    let home = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap());
    assert_eq!(wiki.root(), home.join(".remind-me").join("wiki"));
}
