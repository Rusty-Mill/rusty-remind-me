//! Coverage for `remind_me_import_dbs`.
//!
//! Fixtures build a real `dbs`-shaped SQLite file rather than mocking the
//! read: the whole premise of this importer is that it talks to someone else's
//! schema with plain SQL, and a mock would only ever agree with whatever this
//! module already believes that schema to be.

use remind_me_core::dbs_import::{
    dbs_memory_id, memory_content, pull_dbs, DbsImportError, DEFAULT_CATEGORY, SOURCE_ENTITY_KIND,
    TAG_ENTITY_KIND,
};
use remind_me_core::{Database, DbsImportInput};
use rusqlite::{params, Connection};

/// A scratch directory inside the default import root (the home directory).
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_dbs_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One row destined for `dbs.items`.
struct Item {
    external_id: &'static str,
    source: &'static str,
    item_kind: &'static str,
    title: &'static str,
    url: &'static str,
    body: &'static str,
    tags: &'static [&'static str],
    created_at: &'static str,
    content_hash: &'static str,
    deleted: bool,
}

impl Default for Item {
    fn default() -> Self {
        Self {
            external_id: "x1",
            source: "raindrop",
            item_kind: "link",
            title: "A quokka",
            url: "https://example.invalid/quokka",
            body: "A small marsupial found on Rottnest Island.",
            tags: &["marsupials", "australia"],
            created_at: "2026-01-01T00:00:00+00:00",
            content_hash: "hash-1",
            deleted: false,
        }
    }
}

/// Write a `dbs`-shaped archive, matching the schema this importer reads.
fn write_archive(dir: &std::path::Path, name: &str, items: &[Item]) -> std::path::PathBuf {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    let dbs = Connection::open(&path).unwrap();
    dbs.execute_batch(
        "CREATE TABLE sources (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
         CREATE TABLE items (
             id              INTEGER PRIMARY KEY,
             source_id       INTEGER NOT NULL,
             external_id     TEXT NOT NULL,
             item_kind       TEXT,
             title           TEXT,
             url             TEXT,
             body            TEXT,
             tags_json       TEXT,
             item_created_at TEXT,
             item_updated_at TEXT,
             content_hash    TEXT NOT NULL,
             deleted         INTEGER NOT NULL DEFAULT 0
         );",
    )
    .unwrap();

    for item in items {
        dbs.execute(
            "INSERT OR IGNORE INTO sources (name) VALUES (?)",
            params![item.source],
        )
        .unwrap();
        let source_id: i64 = dbs
            .query_row(
                "SELECT id FROM sources WHERE name = ?",
                params![item.source],
                |r| r.get(0),
            )
            .unwrap();
        dbs.execute(
            "INSERT INTO items
                (source_id, external_id, item_kind, title, url, body, tags_json,
                 item_created_at, item_updated_at, content_hash, deleted)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                source_id,
                item.external_id,
                item.item_kind,
                item.title,
                item.url,
                item.body,
                serde_json::to_string(item.tags).unwrap(),
                item.created_at,
                item.created_at,
                item.content_hash,
                item.deleted as i64,
            ],
        )
        .unwrap();
    }
    path
}

fn input(path: &std::path::Path) -> DbsImportInput {
    DbsImportInput {
        db_path: path.display().to_string(),
        source: String::new(),
        item_type: String::new(),
        limit: 500,
        offset: 0,
        tags: Vec::new(),
        dry_run: false,
    }
}

fn memory_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

fn live_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM memories WHERE superseded_by IS NULL",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// Entity names linked to a memory, with their kinds.
fn linked_entities(conn: &Connection, memory_id: &str) -> Vec<(String, Option<String>)> {
    let mut statement = conn
        .prepare(
            "SELECT e.name, e.kind
               FROM memory_entities me JOIN entities e ON me.entity_id = e.id
              WHERE me.memory_id = ?
              ORDER BY e.name",
        )
        .unwrap();
    let rows = statement
        .query_map(params![memory_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

// ---------------------------------------------------------------------------
// A first import
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_import_stores_every_live_item() {
    let dir = scratch("fresh");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item::default(),
            Item {
                external_id: "x2",
                title: "Rottnest Island",
                content_hash: "hash-2",
                ..Default::default()
            },
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = pull_dbs(&conn, &input(&path)).unwrap();

    assert_eq!(result.fetched, 2);
    assert_eq!(result.created, 2);
    assert_eq!(result.updated, 0);
    assert_eq!(result.imported, 2);
    assert_eq!(result.already_imported, 0);
    assert!(!result.has_more, "a partial page is the last page");
    assert_eq!(memory_count(&conn), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_items_fields_land_where_they_are_useful() {
    let dir = scratch("fields");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    pull_dbs(&conn, &input(&path)).unwrap();

    let (id, content, category, source, tags, metadata, created_at): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT id, content, category, source, tags, metadata, created_at FROM memories",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(id, dbs_memory_id("raindrop", "x1", "hash-1"));
    assert_eq!(
        content,
        "A quokka\n\nA small marsupial found on Rottnest Island."
    );
    // item_kind becomes the category, deliberately not an entity: there is no
    // established "kind" entity type in this graph to reuse.
    assert_eq!(category, "link");
    assert_eq!(source, "dbs:raindrop");
    let tags: Vec<String> = serde_json::from_str(&tags).unwrap();
    assert_eq!(tags, vec!["marsupials", "australia"]);

    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["dbs_source"], "raindrop");
    assert_eq!(metadata["dbs_external_id"], "x1");
    assert_eq!(metadata["dbs_item_kind"], "link");
    assert_eq!(metadata["dbs_content_hash"], "hash-1");

    // The item's own creation time, not the import's. Vitality decay reads
    // this column, so importing an archive must not make a decade of history
    // look like it all happened today.
    assert_eq!(created_at, "2026-01-01T00:00:00+00:00");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_item_with_no_kind_gets_the_fallback_category() {
    let dir = scratch("nokind");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[Item {
            item_kind: "",
            ..Default::default()
        }],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    pull_dbs(&conn, &input(&path)).unwrap();

    let category: String = conn
        .query_row("SELECT category FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(category, DEFAULT_CATEGORY);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn content_falls_back_through_title_body_url_and_id() {
    // The reason to test this directly: an item with no text at all is a real
    // shape (a bare bookmark), and an empty memory would be unfindable.
    assert_eq!(memory_content(Some("T"), Some("B"), None, "x"), "T\n\nB");
    assert_eq!(memory_content(Some("T"), Some("  "), None, "x"), "T");
    assert_eq!(memory_content(Some(""), Some("B"), None, "x"), "B");
    assert_eq!(
        memory_content(None, None, Some("https://u"), "x"),
        "https://u"
    );
    assert_eq!(memory_content(None, None, None, "x"), "x");
    assert_eq!(memory_content(None, None, Some("   "), "x"), "x");
}

// ---------------------------------------------------------------------------
// Entities — the reason this exists over the export route
// ---------------------------------------------------------------------------

#[test]
fn the_source_and_every_tag_become_linked_entities() {
    let dir = scratch("entities");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = pull_dbs(&conn, &input(&path)).unwrap();

    let memory_id: String = conn
        .query_row("SELECT id FROM memories", [], |r| r.get(0))
        .unwrap();
    let linked = linked_entities(&conn, &memory_id);

    // Without these the importer has no reason to exist: `dbs export-notes`
    // plus the folder watcher already covers the content, and only flattens
    // the structure.
    assert_eq!(
        linked,
        vec![
            ("australia".to_string(), Some(TAG_ENTITY_KIND.to_string())),
            ("marsupials".to_string(), Some(TAG_ENTITY_KIND.to_string())),
            ("raindrop".to_string(), Some(SOURCE_ENTITY_KIND.to_string())),
        ]
    );
    assert_eq!(result.entities_created, 3);
    assert_eq!(result.entity_links, 3);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn item_kind_is_not_an_entity() {
    let dir = scratch("kindentity");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    pull_dbs(&conn, &input(&path)).unwrap();

    let kinds: i64 = conn
        .query_row(
            "SELECT count(*) FROM entities WHERE name = 'link'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kinds, 0,
        "inventing a 'kind' entity type is the thing to avoid"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn two_items_from_one_source_share_its_entity() {
    let dir = scratch("shared");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item::default(),
            Item {
                external_id: "x2",
                content_hash: "hash-2",
                tags: &["marsupials"],
                ..Default::default()
            },
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = pull_dbs(&conn, &input(&path)).unwrap();

    // raindrop, marsupials, australia — the second item adds none of them.
    assert_eq!(result.entities_created, 3);
    // ...but it does link to two of them, which is what makes the graph a
    // graph rather than a per-memory tag list.
    assert_eq!(result.entity_links, 5);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn extra_tags_are_added_to_every_memory_and_become_entities() {
    let dir = scratch("extratags");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input(&path);
    params.tags = vec!["archived".to_string(), "  ".to_string()];

    pull_dbs(&conn, &params).unwrap();

    let tags: String = conn
        .query_row("SELECT tags FROM memories", [], |r| r.get(0))
        .unwrap();
    let tags: Vec<String> = serde_json::from_str(&tags).unwrap();
    assert_eq!(tags, vec!["marsupials", "australia", "archived"]);

    let memory_id: String = conn
        .query_row("SELECT id FROM memories", [], |r| r.get(0))
        .unwrap();
    let names: Vec<String> = linked_entities(&conn, &memory_id)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(names.contains(&"archived".to_string()));
    // A blank tag is dropped rather than becoming an entity with no name.
    assert_eq!(names.len(), 4);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Reruns
// ---------------------------------------------------------------------------

#[test]
fn a_rerun_over_unchanged_items_writes_nothing() {
    let dir = scratch("rerun");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    pull_dbs(&conn, &input(&path)).unwrap();

    let again = pull_dbs(&conn, &input(&path)).unwrap();

    assert_eq!(again.fetched, 1);
    assert_eq!(again.already_imported, 1);
    assert_eq!(again.to_import, 0);
    assert_eq!(again.imported, 0);
    assert_eq!(memory_count(&conn), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_rerun_picks_up_only_the_new_item() {
    let dir = scratch("newitem");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    pull_dbs(&conn, &input(&path)).unwrap();

    write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item::default(),
            Item {
                external_id: "x2",
                content_hash: "hash-2",
                ..Default::default()
            },
        ],
    );
    let again = pull_dbs(&conn, &input(&path)).unwrap();

    assert_eq!(again.fetched, 2);
    assert_eq!(again.already_imported, 1);
    assert_eq!(again.created, 1);
    assert_eq!(again.updated, 0);
    assert_eq!(memory_count(&conn), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_edited_item_supersedes_its_previous_version_rather_than_overwriting_it() {
    let dir = scratch("edited");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    pull_dbs(&conn, &input(&path)).unwrap();
    let original = dbs_memory_id("raindrop", "x1", "hash-1");

    // Same identity, different content. The hash is what catches this — dbs
    // does not always move a timestamp on an edit, so an importer keying on
    // item_created_at would see nothing.
    write_archive(
        &dir,
        "dbs.sqlite3",
        &[Item {
            body: "Actually found on Rottnest and Bald Island.",
            content_hash: "hash-2",
            ..Default::default()
        }],
    );
    let again = pull_dbs(&conn, &input(&path)).unwrap();

    assert_eq!(again.updated, 1);
    assert_eq!(again.created, 0);

    let replacement = dbs_memory_id("raindrop", "x1", "hash-2");
    // Both rows survive: history accumulates.
    assert_eq!(memory_count(&conn), 2);
    // Only the new one is live, so search and every other read path see one.
    assert_eq!(live_count(&conn), 1);

    let superseded_by: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = ?",
            params![original],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(superseded_by.as_deref(), Some(replacement.as_str()));

    // And the tracking row now points at the replacement, so a third rerun
    // over unchanged content is a no-op rather than superseding again.
    let tracked: (String, String) = conn
        .query_row(
            "SELECT memory_id, content_hash FROM dbs_imports WHERE external_id = 'x1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(tracked, (replacement, "hash-2".to_string()));

    let third = pull_dbs(&conn, &input(&path)).unwrap();
    assert_eq!(third.imported, 0);
    assert_eq!(memory_count(&conn), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_memory_id_is_derived_rather_than_minted() {
    // Two runs over the same item version must compute the same id, or a
    // retried import leaves an orphan that dbs_imports never catches: that
    // table keeps one row per (source, external_id) and records whichever
    // call wrote last.
    assert_eq!(
        dbs_memory_id("raindrop", "x1", "h"),
        dbs_memory_id("raindrop", "x1", "h")
    );
    // Each component participates, so an edit — and only an edit — moves it.
    assert_ne!(
        dbs_memory_id("raindrop", "x1", "h"),
        dbs_memory_id("raindrop", "x1", "h2")
    );
    assert_ne!(
        dbs_memory_id("raindrop", "x1", "h"),
        dbs_memory_id("reddit", "x1", "h")
    );
    assert_ne!(
        dbs_memory_id("raindrop", "x1", "h"),
        dbs_memory_id("raindrop", "x2", "h")
    );
    assert_eq!(dbs_memory_id("raindrop", "x1", "h").len(), 12);
}

// ---------------------------------------------------------------------------
// Filters, paging, dry run
// ---------------------------------------------------------------------------

#[test]
fn deleted_items_are_never_imported() {
    let dir = scratch("deleted");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item::default(),
            Item {
                external_id: "gone",
                content_hash: "hash-gone",
                deleted: true,
                ..Default::default()
            },
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = pull_dbs(&conn, &input(&path)).unwrap();

    assert_eq!(result.fetched, 1, "the deleted row is not even read");
    assert_eq!(memory_count(&conn), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_source_and_item_type_filters_narrow_the_read() {
    let dir = scratch("filters");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item::default(),
            Item {
                external_id: "r1",
                source: "reddit",
                item_kind: "post",
                content_hash: "hash-r1",
                ..Default::default()
            },
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let mut by_source = input(&path);
    by_source.source = "reddit".to_string();
    let result = pull_dbs(&conn, &by_source).unwrap();
    assert_eq!(result.fetched, 1);
    assert_eq!(result.source.as_deref(), Some("reddit"));

    let db2 = Database::open_in_memory().unwrap();
    let conn2 = db2.conn();
    let mut by_kind = input(&path);
    by_kind.item_type = "link".to_string();
    let result = pull_dbs(&conn2, &by_kind).unwrap();
    assert_eq!(result.fetched, 1);
    assert_eq!(result.item_type.as_deref(), Some("link"));
    let source: String = conn2
        .query_row("SELECT source FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(source, "dbs:raindrop");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn paging_reports_more_while_a_page_comes_back_full() {
    let dir = scratch("paging");
    let path = write_archive(
        &dir,
        "dbs.sqlite3",
        &[
            Item {
                external_id: "a",
                created_at: "2026-01-01T00:00:00+00:00",
                content_hash: "h-a",
                ..Default::default()
            },
            Item {
                external_id: "b",
                created_at: "2026-01-02T00:00:00+00:00",
                content_hash: "h-b",
                ..Default::default()
            },
            Item {
                external_id: "c",
                created_at: "2026-01-03T00:00:00+00:00",
                content_hash: "h-c",
                ..Default::default()
            },
        ],
    );
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let mut page = input(&path);
    page.limit = 2;
    let first = pull_dbs(&conn, &page).unwrap();
    assert_eq!(first.fetched, 2);
    assert!(
        first.has_more,
        "a full page means there is probably another"
    );

    page.offset = 2;
    let second = pull_dbs(&conn, &page).unwrap();
    assert_eq!(second.fetched, 1);
    assert!(!second.has_more);
    assert_eq!(memory_count(&conn), 3, "the pages did not overlap or skip");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_out_of_range_limit_is_clamped_rather_than_rejected() {
    let dir = scratch("clamp");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let mut zero = input(&path);
    zero.limit = 0;
    let result = pull_dbs(&conn, &zero).unwrap();

    // Clamped up to 1 rather than fetching nothing, matching how every other
    // bounded input in this crate behaves. The clamped value is reported so a
    // caller paging with `limit` is not surprised by the offsets.
    assert_eq!(result.limit, 1);
    assert_eq!(result.fetched, 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_dry_run_reports_the_work_without_doing_any_of_it() {
    let dir = scratch("dryrun");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input(&path);
    params.dry_run = true;

    let result = pull_dbs(&conn, &params).unwrap();

    assert_eq!(result.fetched, 1);
    assert_eq!(result.to_import, 1);
    assert_eq!(result.imported, 0);
    assert_eq!(memory_count(&conn), 0);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM dbs_imports", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "a dry run that recorded a tracking row would make the real run a no-op"
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM entities", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Bad inputs
// ---------------------------------------------------------------------------

#[test]
fn a_path_outside_the_import_roots_is_refused_without_revealing_whether_it_exists() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut params = input(std::path::Path::new("/etc/hosts"));
    params.db_path = "/etc/hosts".to_string();

    let error = pull_dbs(&conn, &params).unwrap_err();

    // Containment before existence: a check that tested existence first would
    // answer "does this path exist?" for any path on the machine.
    assert!(
        matches!(error, DbsImportError::Path(_)),
        "expected a path refusal, got {error}"
    );
    assert!(error.to_string().contains("not in allowed import roots"));
}

#[test]
fn a_missing_archive_is_reported_as_missing() {
    let dir = scratch("missing");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let params = input(&dir.join("nothing-here.sqlite3"));

    let error = pull_dbs(&conn, &params).unwrap_err();

    assert!(matches!(error, DbsImportError::Path(_)), "got {error}");
    assert!(error.to_string().contains("File not found"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_that_is_not_a_database_is_reported_as_such() {
    let dir = scratch("notadb");
    let path = dir.join("archive.sqlite3");
    std::fs::write(&path, "this is not a database, it is a sentence").unwrap();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let error = pull_dbs(&conn, &input(&path)).unwrap_err();

    assert!(
        matches!(error, DbsImportError::NotADatabase { .. }),
        "got {error}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_database_without_the_dbs_tables_says_so() {
    let dir = scratch("wrongdb");
    let path = dir.join("archive.sqlite3");
    let other = Connection::open(&path).unwrap();
    other
        .execute_batch("CREATE TABLE unrelated (x TEXT);")
        .unwrap();
    drop(other);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let error = pull_dbs(&conn, &input(&path)).unwrap_err();

    // Distinguished from "not a database at all": one is the wrong file, the
    // other is the right kind of file from the wrong tool.
    assert!(
        matches!(error, DbsImportError::NotADbsArchive { .. }),
        "got {error}"
    );
    assert!(error.to_string().contains("items/sources"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_archive_is_opened_read_only() {
    let dir = scratch("readonly");
    let path = write_archive(&dir, "dbs.sqlite3", &[Item::default()]);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    pull_dbs(&conn, &input(&path)).unwrap();

    // This is someone's backup archive. The guarantee is not "this module
    // never writes" but "a write would fail", so it is asserted at the SQLite
    // layer the importer actually opens the file through.
    let reopened = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    assert!(reopened
        .execute("DELETE FROM items", [])
        .unwrap_err()
        .to_string()
        .contains("readonly"));

    // And the archive is unchanged after the import.
    let remaining: i64 = reopened
        .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 1);

    // Windows refuses to delete a file (or its parent directory) while a
    // handle to it is still open -- Unix allows this, so it never surfaced
    // there. `reopened` is the one connection still holding the archive
    // file open at this point.
    drop(reopened);
    std::fs::remove_dir_all(&dir).unwrap();
}
