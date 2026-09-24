//! Coverage for `remind_me_import_mempalace`.
//!
//! Fixtures build a real Chroma-shaped SQLite file — the metadata segment
//! tables verified in `docs/adr/0001-mempalace-chroma-sqlite-read.md` against
//! `chromadb` 0.5.0 and 1.5.9 — rather than mocking the read. The vector
//! segment is never created at all: the whole point of this importer is that
//! it never needs one.
//!
//! `REMIND_ME_MEMPALACE_PATH` is process-global environment state, and tests
//! run concurrently by default, so every test that touches it holds `ENV_LOCK`
//! for the duration — the standard way to make an env-var-configured surface
//! testable without changing its public shape to take a path argument it
//! deliberately does not have (see the ADR: the store location is operator
//! configuration, not a per-call parameter).

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::mempalace_import::{
    parse_frontmatter, pull_mempalace, MempalaceImportError, COLLECTION_NAME, DEFAULT_CATEGORY,
    OPAQUE_SOURCE,
};
use remind_me_core::{Database, MempalaceImportInput};
use rusqlite::{params, Connection};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_mempalace_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One drawer destined for the metadata segment: (drawer_id, document, wing, room).
type Drawer<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>);

/// Write a Chroma-shaped store with one collection ("mempalace_drawers"),
/// its vector and metadata segments, and the given drawers in the metadata
/// segment. The vector segment row exists (a real collection always has one)
/// but nothing is ever written under it — there is no HNSW index here,
/// deliberately: this importer must never need one to work.
fn write_store(dir: &std::path::Path, drawers: &[Drawer]) {
    let path = dir.join("chroma.sqlite3");
    let _ = std::fs::remove_file(&path);
    let chroma = Connection::open(&path).unwrap();
    chroma
        .execute_batch(
            "CREATE TABLE collections (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, dimension INTEGER);
             CREATE TABLE segments (id TEXT PRIMARY KEY, type TEXT NOT NULL, scope TEXT NOT NULL, collection TEXT);
             CREATE TABLE embeddings (
                 id INTEGER PRIMARY KEY,
                 segment_id TEXT NOT NULL,
                 embedding_id TEXT NOT NULL,
                 seq_id BLOB NOT NULL,
                 created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                 UNIQUE (segment_id, embedding_id)
             );
             CREATE TABLE embedding_metadata (
                 id INTEGER REFERENCES embeddings(id),
                 key TEXT NOT NULL,
                 string_value TEXT,
                 int_value INTEGER,
                 float_value REAL,
                 bool_value INTEGER,
                 PRIMARY KEY (id, key)
             );
             INSERT INTO collections (id, name, dimension) VALUES ('col-1', 'mempalace_drawers', 384);
             INSERT INTO segments (id, type, scope, collection) VALUES ('seg-vector', 'hnsw-local-persisted', 'VECTOR', 'col-1');
             INSERT INTO segments (id, type, scope, collection) VALUES ('seg-meta', 'sqlite', 'METADATA', 'col-1');",
        )
        .unwrap();

    for (drawer_id, document, wing, room) in drawers {
        chroma
            .execute(
                "INSERT INTO embeddings (segment_id, embedding_id, seq_id) VALUES ('seg-meta', ?, x'00')",
                params![drawer_id],
            )
            .unwrap();
        let embeddings_id = chroma.last_insert_rowid();
        chroma
            .execute(
                "INSERT INTO embedding_metadata (id, key, string_value) VALUES (?, 'chroma:document', ?)",
                params![embeddings_id, document],
            )
            .unwrap();
        if let Some(w) = wing {
            chroma
                .execute(
                    "INSERT INTO embedding_metadata (id, key, string_value) VALUES (?, 'wing', ?)",
                    params![embeddings_id, w],
                )
                .unwrap();
        }
        if let Some(r) = room {
            chroma
                .execute(
                    "INSERT INTO embedding_metadata (id, key, string_value) VALUES (?, 'room', ?)",
                    params![embeddings_id, r],
                )
                .unwrap();
        }
    }
}

fn input() -> MempalaceImportInput {
    MempalaceImportInput {
        wing: String::new(),
        room: String::new(),
        limit: 500,
        offset: 0,
        category: String::new(),
        tags: Vec::new(),
        dry_run: false,
    }
}

fn memory_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

const NATIVE_DOCUMENT: &str = "---\ncategory: fact\nsource: remind_me/manual\ntags: work, deadline\ncreated: 2025-06-01T00:00:00Z\n---\n\nThe deploy window is Tuesdays.";

/// Run `body` with `REMIND_ME_MEMPALACE_PATH` pointed at `dir`, holding the
/// process-wide env lock for the duration.
fn with_store_at<R>(dir: &std::path::Path, body: impl FnOnce() -> R) -> R {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var("REMIND_ME_MEMPALACE_PATH", dir);
    let result = body();
    crate::test_env::remove_var("REMIND_ME_MEMPALACE_PATH");
    result
}

// ---------------------------------------------------------------------------
// parse_frontmatter
// ---------------------------------------------------------------------------

#[test]
fn frontmatter_fields_and_body_are_split() {
    let (fields, body) = parse_frontmatter(NATIVE_DOCUMENT).unwrap();
    assert_eq!(fields.get("category").unwrap(), "fact");
    assert_eq!(fields.get("source").unwrap(), "remind_me/manual");
    assert_eq!(fields.get("tags").unwrap(), "work, deadline");
    assert_eq!(body, "The deploy window is Tuesdays.");
}

#[test]
fn opaque_content_has_no_frontmatter() {
    assert!(parse_frontmatter("just some plain drawer content").is_none());
    assert!(parse_frontmatter("").is_none());
}

#[test]
fn a_value_containing_a_colon_is_kept_whole() {
    let doc = "---\nsource: remind_me/manual:extra\n---\n\nbody";
    let (fields, _) = parse_frontmatter(doc).unwrap();
    assert_eq!(fields.get("source").unwrap(), "remind_me/manual:extra");
}

#[test]
fn a_field_block_with_no_closing_delimiter_does_not_match() {
    assert!(parse_frontmatter("---\ncategory: fact\nno closing delimiter here").is_none());
}

#[test]
fn a_key_with_digits_does_not_match_the_reserved_charset() {
    // The reference's regex key class is [a-zA-Z_]+ only.
    assert!(parse_frontmatter("---\ncat2: fact\n---\n\nbody").is_none());
}

// ---------------------------------------------------------------------------
// Store absent / not a database / no collection
// ---------------------------------------------------------------------------

#[test]
fn a_missing_store_is_reported_as_missing() {
    let dir = scratch("missing");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let error = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap_err());

    assert!(
        matches!(error, MempalaceImportError::NotFound { .. }),
        "{error}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_that_is_not_a_database_is_reported_as_such() {
    let dir = scratch("notadb");
    std::fs::write(dir.join("chroma.sqlite3"), "not a sqlite file").unwrap();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let error = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap_err());

    assert!(
        matches!(error, MempalaceImportError::NotADatabase { .. }),
        "{error}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_database_with_no_mempalace_drawers_collection_says_so() {
    let dir = scratch("nocollection");
    let chroma = Connection::open(dir.join("chroma.sqlite3")).unwrap();
    chroma
        .execute_batch("CREATE TABLE collections (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE);")
        .unwrap();
    drop(chroma);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let error = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap_err());

    assert!(
        matches!(error, MempalaceImportError::NoCollection { .. }),
        "{error}"
    );
    assert!(error.to_string().contains(COLLECTION_NAME));
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Frontmatter restore vs opaque
// ---------------------------------------------------------------------------

#[test]
fn a_native_drawer_restores_category_tags_source_and_created() {
    let dir = scratch("native");
    write_store(
        &dir,
        &[("drawer-1", NATIVE_DOCUMENT, Some("acme"), Some("ops"))],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    assert_eq!(result.native_format, 1);
    assert_eq!(result.opaque_format, 0);
    assert_eq!(result.imported, 1);

    let (content, category, source, tags, created_at): (String, String, String, String, String) =
        conn.query_row(
            "SELECT content, category, source, tags, created_at FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();

    assert_eq!(content, "The deploy window is Tuesdays.");
    assert_eq!(category, "fact");
    // Restored, but prefixed -- matching the reference's actual behaviour,
    // not a literal restore of the frontmatter's raw source value.
    assert_eq!(source, "mempalace:remind_me/manual");
    let tags: Vec<String> = serde_json::from_str(&tags).unwrap();
    assert_eq!(tags, vec!["work", "deadline"]);
    assert_eq!(created_at, "2025-06-01T00:00:00Z");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_native_drawers_frontmatter_id_is_not_restored() {
    // The reference parses `fields["id"]` and never reads it again -- the
    // memory id is always freshly minted. Matched here deliberately, not
    // "fixed": this crate tracks the reference's actual behaviour, and the
    // issue's "restore original id" is not what pull_mempalace does.
    let dir = scratch("native-id");
    let doc = "---\nid: original-frontmatter-id\ncategory: fact\n---\n\nbody text";
    write_store(&dir, &[("drawer-1", doc, None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    let id: String = conn
        .query_row("SELECT id FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_ne!(id, "original-frontmatter-id");
    assert!(id.starts_with("mem_"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_opaque_drawer_is_stored_as_is_tagged_with_wing_and_room() {
    let dir = scratch("opaque");
    write_store(
        &dir,
        &[(
            "drawer-1",
            "just plain drawer text",
            Some("acme"),
            Some("ops"),
        )],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    assert_eq!(result.opaque_format, 1);
    assert_eq!(result.native_format, 0);

    let (content, category, source, tags): (String, String, String, String) = conn
        .query_row(
            "SELECT content, category, source, tags FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(content, "just plain drawer text");
    assert_eq!(category, DEFAULT_CATEGORY);
    assert_eq!(source, OPAQUE_SOURCE);
    let tags: Vec<String> = serde_json::from_str(&tags).unwrap();
    assert_eq!(tags, vec!["acme", "ops"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn extra_tags_are_appended_to_both_native_and_opaque_drawers() {
    let dir = scratch("extra-tags");
    write_store(
        &dir,
        &[
            ("drawer-1", NATIVE_DOCUMENT, None, None),
            ("drawer-2", "opaque text", Some("acme"), None),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.tags = vec!["archived".to_string()];

    with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    let mut stmt = conn
        .prepare("SELECT tags FROM memories ORDER BY content")
        .unwrap();
    let all_tags: Vec<Vec<String>> = stmt
        .query_map([], |r| {
            let raw: String = r.get(0)?;
            Ok(serde_json::from_str(&raw).unwrap())
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for tags in &all_tags {
        assert!(tags.contains(&"archived".to_string()), "{:?}", tags);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_caller_supplied_category_only_applies_to_drawers_without_one() {
    let dir = scratch("category-fallback");
    // Native drawer's own frontmatter category wins over the caller's.
    write_store(
        &dir,
        &[
            ("drawer-1", NATIVE_DOCUMENT, None, None),
            ("drawer-2", "opaque text", None, None),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.category = "caller_category".to_string();

    with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    let mut stmt = conn
        .prepare("SELECT content, category FROM memories ORDER BY content")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let by_content: std::collections::HashMap<_, _> = rows.into_iter().collect();
    assert_eq!(by_content["The deploy window is Tuesdays."], "fact");
    assert_eq!(by_content["opaque text"], "caller_category");

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Wing / room filters
// ---------------------------------------------------------------------------

#[test]
fn wing_filters_to_matching_drawers_only() {
    let dir = scratch("wing-filter");
    write_store(
        &dir,
        &[
            ("d1", "one", Some("acme"), Some("ops")),
            ("d2", "two", Some("other"), Some("ops")),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.wing = "acme".to_string();

    let result = with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    assert_eq!(result.fetched, 1);
    assert_eq!(result.wing.as_deref(), Some("acme"));
    assert_eq!(memory_count(&conn), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn room_filters_within_a_wing() {
    let dir = scratch("room-filter");
    write_store(
        &dir,
        &[
            ("d1", "one", Some("acme"), Some("ops")),
            ("d2", "two", Some("acme"), Some("eng")),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.wing = "acme".to_string();
    params.room = "eng".to_string();

    let result = with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    assert_eq!(result.fetched, 1);
    let content: String = conn
        .query_row("SELECT content FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "two");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_drawer_with_no_wing_metadata_does_not_match_a_wing_filter() {
    let dir = scratch("no-wing");
    write_store(&dir, &[("d1", "one", None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.wing = "acme".to_string();

    let result = with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    assert_eq!(result.fetched, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[test]
fn paging_reports_more_while_a_page_comes_back_full() {
    let dir = scratch("paging");
    write_store(
        &dir,
        &[
            ("d0", "content zero", None, None),
            ("d1", "content one", None, None),
            ("d2", "content two", None, None),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut page = input();
    page.limit = 2;

    let first = with_store_at(&dir, || pull_mempalace(&conn, &page).unwrap());
    assert_eq!(first.fetched, 2);
    assert!(first.has_more);

    page.offset = 2;
    let second = with_store_at(&dir, || pull_mempalace(&conn, &page).unwrap());
    assert_eq!(second.fetched, 1);
    assert!(!second.has_more);

    assert_eq!(memory_count(&conn), 3, "the pages did not overlap or skip");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_out_of_range_limit_is_clamped_rather_than_rejected() {
    let dir = scratch("clamp");
    write_store(&dir, &[("d1", "content", None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.limit = 0;

    let result = with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    assert_eq!(result.limit, 1);
    assert_eq!(result.fetched, 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Rerun dedup
// ---------------------------------------------------------------------------

#[test]
fn a_rerun_over_the_same_drawers_imports_nothing_new() {
    let dir = scratch("rerun");
    write_store(&dir, &[("d1", "content one", None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    let again = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    assert_eq!(again.fetched, 1);
    assert_eq!(again.already_imported, 1);
    assert_eq!(again.to_import, 0);
    assert_eq!(again.imported, 0);
    assert_eq!(memory_count(&conn), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_rerun_after_adding_a_drawer_imports_only_the_new_one() {
    let dir = scratch("rerun-new");
    write_store(&dir, &[("d1", "content one", None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    write_store(
        &dir,
        &[
            ("d1", "content one", None, None),
            ("d2", "content two", None, None),
        ],
    );
    let result = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    assert_eq!(result.already_imported, 1);
    assert_eq!(result.to_import, 1);
    assert_eq!(memory_count(&conn), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Unlike `dbs_import`, there is no edit-detection: a drawer is dedup'd by id
/// alone, so an edited drawer that keeps its id is silently skipped on a
/// rerun. This matches the reference exactly -- it has no content-hash
/// column to notice the edit with -- and is asserted directly so a future
/// change does not accidentally "fix" this into a divergence from upstream.
#[test]
fn an_edited_drawer_keeping_its_id_is_not_reimported() {
    let dir = scratch("edited");
    write_store(&dir, &[("d1", "original content", None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    write_store(&dir, &[("d1", "edited content", None, None)]);
    let result = with_store_at(&dir, || pull_mempalace(&conn, &input()).unwrap());

    assert_eq!(result.to_import, 0);
    let content: String = conn
        .query_row("SELECT content FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "original content");

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_reports_the_work_without_doing_any_of_it() {
    let dir = scratch("dryrun");
    write_store(&dir, &[("d1", NATIVE_DOCUMENT, None, None)]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input();
    params.dry_run = true;

    let result = with_store_at(&dir, || pull_mempalace(&conn, &params).unwrap());

    assert_eq!(result.to_import, 1);
    assert_eq!(result.native_format, 1);
    assert_eq!(result.imported, 0);
    assert_eq!(memory_count(&conn), 0);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM mempalace_imports", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "a dry run that recorded a tracking row would make the real run a no-op"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
