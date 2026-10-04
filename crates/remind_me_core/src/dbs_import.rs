//! Bulk import from a `dbs` (daily-backup-system) archive.
//!
//! [`dbs`] pulls a person's data out of the services that hold it — Reddit,
//! YouTube, Raindrop, GitHub stars — into one SQLite database under a uniform
//! `items`/`sources` schema. This reads that database directly and turns each
//! live item into a memory.
//!
//! [`dbs`]: https://github.com/baileyrd/daily-backup-system
//!
//! # Why this exists when the folder watcher already could
//!
//! `dbs export-notes` writes markdown, and the watcher would happily ingest
//! it. What that route cannot preserve is structure: an item's source and tags
//! arrive as prose in a note, and prose is not a graph. Here they become
//! first-class entities (`FT-04`) linked to the memory, so "everything from
//! raindrop" and "everything tagged rust" are traversals rather than searches.
//! **An implementation of this that did not write entities would have no
//! reason to exist**, since the export route already covers the rest.
//!
//! `item_kind` is deliberately *not* an entity. It becomes the memory's
//! category and lands in metadata, because there is no established "kind"
//! entity type in this graph to reuse and inventing one would put a second,
//! incompatible taxonomy next to the existing ones.
//!
//! # No dependency on `dbs`
//!
//! Its schema is stable and documented, and it is read with plain SQL,
//! **read-only**. That is not a stylistic choice: the file is someone's backup
//! archive, and this crate should not be able to damage it even by accident.
//! The connection is opened with `SQLITE_OPEN_READ_ONLY`, so a write would
//! fail at the SQLite layer rather than relying on this module never
//! attempting one.
//!
//! # Reruns, and what happens to an edit
//!
//! Dedup is keyed on `(dbs_source, external_id)` — `dbs`'s own item identity —
//! tracked in `dbs_imports`. A rerun re-reads everything and writes only what
//! changed.
//!
//! An item whose `content_hash` moved gets a **fresh memory**, and the
//! previous one is marked `superseded_by` it. History accumulates rather than
//! being overwritten, which mirrors what the folder watcher does with a
//! changed file. This is also the point of comparing hashes at all: `dbs`
//! records edits under timestamps that are not always reliable, so an importer
//! keying on `item_created_at` would miss them. A hash comparison does not
//! care which timestamp the edit was filed under.

use crate::db::imports::{DbsTracked as Tracked, ImportLedger};
use crate::db::memories::{Memories, NewMemory};
use crate::db::Store;
use crate::entity::{link_memory_entity, upsert_entity};
use crate::import_paths::{validate_import_database, ImportPathError};
use crate::models::{DbsImportInput, EntityInput, DBS_IMPORT_LIMIT_MAX, DBS_IMPORT_LIMIT_MIN};
use chrono::Utc;
use crate::db::legacy_sqlite::LegacyDb;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// `kind` given to the entity standing for a `dbs` source.
pub const SOURCE_ENTITY_KIND: &str = "dbs_source";
/// `kind` given to the entity standing for one of an item's tags.
pub const TAG_ENTITY_KIND: &str = "tag";

/// `category` for an item `dbs` recorded without an `item_kind`.
pub const DEFAULT_CATEGORY: &str = "dbs_import";

/// Ids per `IN (...)` lookup.
///
/// SQLite's bound-parameter ceiling is far above this on any build this runs
/// on, so the batching is not load-bearing today — it is here so that raising
/// [`DBS_IMPORT_LIMIT_MAX`] later cannot quietly reintroduce a cliff.
const LOOKUP_BATCH: usize = 500;

/// Why an import could not run.
#[derive(Debug)]
pub enum DbsImportError {
    /// The path was refused before anything was opened.
    Path(ImportPathError),
    /// The file is not a readable SQLite database.
    NotADatabase {
        path: String,
        detail: String,
    },
    /// It is a database, but not a `dbs` one.
    NotADbsArchive {
        path: String,
        detail: String,
    },
    /// The node's own store failed, or the archive could not be read.
    Store(crate::db::StoreError),
}

impl std::fmt::Display for DbsImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(e) => write!(f, "{}", e),
            Self::NotADatabase { path, detail } => {
                write!(f, "Not a readable SQLite database: {} ({})", path, detail)
            }
            Self::NotADbsArchive { path, detail } => write!(
                f,
                "Not a dbs archive: {} has no items/sources tables ({})",
                path, detail
            ),
            Self::Store(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for DbsImportError {}

impl From<ImportPathError> for DbsImportError {
    fn from(e: ImportPathError) -> Self {
        Self::Path(e)
    }
}

impl From<crate::db::StoreError> for DbsImportError {
    fn from(e: crate::db::StoreError) -> Self {
        Self::Store(e)
    }
}

/// What one page of an import did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbsImportResult {
    /// The source filter that was applied, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    /// Live items this page read.
    pub fetched: usize,
    /// Of those, how many were already stored unchanged.
    pub already_imported: usize,
    pub to_import: usize,
    pub offset: usize,
    pub limit: usize,
    /// The page was full, so there is probably another one.
    pub has_more: bool,
    /// Items seen for the first time.
    pub created: usize,
    /// Items whose content changed since the last import.
    pub updated: usize,
    /// `created + updated`.
    pub imported: usize,
    /// New source/tag entities recorded.
    pub entities_created: usize,
    /// New memory-to-entity links.
    pub entity_links: usize,
}

/// One row of `dbs.items`, joined to its source.
struct DbsItem {
    external_id: String,
    item_kind: Option<String>,
    title: Option<String>,
    url: Option<String>,
    body: Option<String>,
    tags_json: Option<String>,
    item_created_at: Option<String>,
    content_hash: String,
    source_name: String,
}

/// Open a `dbs` archive read-only.
///
/// The reader touches the file at once: a caller that passes a JPEG should
/// learn that here, not several layers into the import.
fn open_dbs(path: &Path) -> std::result::Result<LegacyDb, DbsImportError> {
    LegacyDb::open(path).map_err(|e| DbsImportError::NotADatabase {
        path: path.display().to_string(),
        detail: e.to_string(),
    })
}

/// The deterministic id for one version of one `dbs` item.
///
/// Everything else in this crate mints `mem_<uuid>`, deliberately unique so
/// that storing the same content twice gives two memories. This is the
/// opposite on purpose: two concurrent or retried imports of the same item
/// version compute the *same* id, so `INSERT OR IGNORE` collapses them into
/// one row. Without that, both calls would read "not yet imported", both would
/// insert under different ids, and `dbs_imports` — which keeps one row per
/// `(source, external_id)` — would record only whichever wrote last, leaving
/// the other memory orphaned and untracked forever.
///
/// A real edit changes `content_hash`, which changes this id, which is exactly
/// what makes the supersession path fire.
pub fn dbs_memory_id(dbs_source: &str, external_id: &str, content_hash: &str) -> String {
    sha256::digest(format!(
        "dbs:{}:{}:{}",
        dbs_source, external_id, content_hash
    ))[..12]
        .to_string()
}

/// Compose a memory's content from an item's fields.
///
/// Falls through to the url and then the external id so an item with no text
/// at all — a bare bookmark, say — still becomes something identifiable rather
/// than an empty memory.
pub fn memory_content(
    title: Option<&str>,
    body: Option<&str>,
    url: Option<&str>,
    external_id: &str,
) -> String {
    let title = title.unwrap_or_default().trim();
    let body = body.unwrap_or_default().trim();
    match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{}\n\n{}", title, body),
        (false, true) => title.to_string(),
        (true, false) => body.to_string(),
        (true, true) => {
            let url = url.unwrap_or_default().trim();
            if url.is_empty() {
                external_id.to_string()
            } else {
                url.to_string()
            }
        }
    }
}

/// Read one page of live items from the archive.
fn read_items(
    dbs: &LegacyDb,
    input: &DbsImportInput,
    limit: usize,
) -> std::result::Result<Vec<DbsItem>, DbsImportError> {
    let mut where_sql = String::from("i.deleted = 0");
    let mut binds: Vec<serde_json::Value> = Vec::new();
    if !input.source.is_empty() {
        where_sql.push_str(" AND s.name = ?");
        binds.push(input.source.clone().into());
    }
    if !input.item_type.is_empty() {
        where_sql.push_str(" AND i.item_kind = ?");
        binds.push(input.item_type.clone().into());
    }

    // Ordered by creation then id so paging is stable: without a total order,
    // two pages of a rerun can overlap or skip items.
    let sql = format!(
        "SELECT i.external_id, i.item_kind, i.title, i.url, i.body,
                i.tags_json, i.item_created_at, i.content_hash, s.name AS source_name
           FROM items i JOIN sources s ON i.source_id = s.id
          WHERE {}
          ORDER BY i.item_created_at, i.external_id
          LIMIT ? OFFSET ?",
        where_sql
    );
    binds.push((limit as i64).into());
    binds.push((input.offset as i64).into());

    // A query that fails to prepare is a database without these tables.
    let rows = dbs
        .query(&sql, &binds)
        .map_err(|e| DbsImportError::NotADbsArchive {
            path: String::new(),
            detail: e.to_string(),
        })?;
    let text = |row: &crate::db::legacy_sqlite::Row, column: &str| -> Option<String> {
        row.get(column).and_then(|v| v.as_str()).map(str::to_string)
    };
    let required = |row: &crate::db::legacy_sqlite::Row, column: &str| {
        text(row, column).ok_or_else(|| DbsImportError::NotADbsArchive {
            path: String::new(),
            detail: format!("items.{column} is missing or not text"),
        })
    };
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(DbsItem {
            external_id: required(row, "external_id")?,
            item_kind: text(row, "item_kind"),
            title: text(row, "title"),
            url: text(row, "url"),
            body: text(row, "body"),
            tags_json: text(row, "tags_json"),
            item_created_at: text(row, "item_created_at"),
            content_hash: required(row, "content_hash")?,
            source_name: required(row, "source_name")?,
        });
    }
    Ok(items)
}

/// What previous imports recorded for the items on this page.
fn tracked_state(
    store: &Store<'_>,
    items: &[DbsItem],
) -> crate::db::Result<HashMap<(String, String), Tracked>> {
    let mut by_source: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in items {
        by_source
            .entry(item.source_name.as_str())
            .or_default()
            .push(item.external_id.as_str());
    }

    let mut tracked = HashMap::new();
    for (source, external_ids) in by_source {
        for batch in external_ids.chunks(LOOKUP_BATCH) {
            tracked.extend(ImportLedger::new(store).dbs_tracked(source, batch)?);
        }
    }
    Ok(tracked)
}

/// An item's tags: its own, then the caller's, with blanks dropped.
fn item_tags(item: &DbsItem, extra: &[String]) -> Vec<String> {
    let own: Vec<String> = item
        .tags_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .filter(|t| !t.trim().is_empty())
        .collect();

    own.into_iter()
        .chain(extra.iter().cloned())
        .filter(|t| !t.trim().is_empty())
        .collect()
}

/// Pull a page of `dbs` items into memory and the entity graph.
///
/// Reruns are safe and are the intended way to use this: unchanged items are
/// counted and skipped, new ones are imported, and edited ones supersede their
/// previous version. Page through a large archive by raising `offset` until
/// `has_more` is false.
///
/// `limit` is clamped to [`DBS_IMPORT_LIMIT_MIN`]..=[`DBS_IMPORT_LIMIT_MAX`]
/// rather than rejected, matching how every other bounded input in this crate
/// behaves.
///
/// The whole page is written in one transaction. A half-applied import would
/// leave `dbs_imports` claiming items that no memory backs, and the next rerun
/// would skip them — the archive would look imported when it was not.
pub fn pull_dbs(
    store: &Store<'_>,
    input: &DbsImportInput,
) -> std::result::Result<DbsImportResult, DbsImportError> {
    let path = validate_import_database(&input.db_path)?;
    let limit = input
        .limit
        .clamp(DBS_IMPORT_LIMIT_MIN, DBS_IMPORT_LIMIT_MAX);

    let items = {
        let dbs = open_dbs(&path)?;
        read_items(&dbs, input, limit).map_err(|e| match e {
            // `read_items` cannot name the file, so it is filled in here.
            DbsImportError::NotADbsArchive { detail, .. } => DbsImportError::NotADbsArchive {
                path: path.display().to_string(),
                detail,
            },
            other => other,
        })?
        // The archive connection closes here, before anything is written.
    };

    let fetched = items.len();
    let tracked = tracked_state(store, &items)?;

    let mut result = DbsImportResult {
        source: Some(input.source.clone()).filter(|s| !s.is_empty()),
        item_type: Some(input.item_type.clone()).filter(|s| !s.is_empty()),
        fetched,
        offset: input.offset,
        limit,
        has_more: fetched == limit,
        ..Default::default()
    };

    let mut to_import = Vec::new();
    for item in &items {
        let key = (item.source_name.clone(), item.external_id.clone());
        match tracked.get(&key) {
            Some(prior) if prior.content_hash == item.content_hash => result.already_imported += 1,
            _ => to_import.push(item),
        }
    }
    result.to_import = to_import.len();

    if input.dry_run {
        return Ok(result);
    }

    let now = Utc::now().to_rfc3339();
    // One transaction for the page: a SQLite transaction, and one journal
    // batch on the engine's memories core (ADR-0023, core PR 4b).
    store.transaction(|page| -> Result<(), DbsImportError> {
        for item in to_import {
            let key = (item.source_name.clone(), item.external_id.clone());
            let prior = tracked.get(&key);
            let tags = item_tags(item, &input.tags);
            let content = memory_content(
                item.title.as_deref(),
                item.body.as_deref(),
                item.url.as_deref(),
                &item.external_id,
            );
            let memory_id = dbs_memory_id(&item.source_name, &item.external_id, &item.content_hash);

            let metadata = serde_json::json!({
                "dbs_source": item.source_name,
                "dbs_external_id": item.external_id,
                "dbs_item_kind": item.item_kind,
                "dbs_url": item.url,
                "dbs_content_hash": item.content_hash,
            });

            let provenance = crate::context::importer_provenance("dbs");
            Memories::new(page).insert_or_ignore(&NewMemory {
                category: item
                    .item_kind
                    .as_deref()
                    .filter(|k| !k.is_empty())
                    .unwrap_or(DEFAULT_CATEGORY)
                    .to_string(),
                tags: tags.clone(),
                source: format!("dbs:{}", item.source_name),
                metadata,
                // The item's own creation time, so a memory ages from when the
                // thing happened rather than from when it was imported —
                // vitality decay reads this column.
                created_at: item.item_created_at.clone().unwrap_or_else(|| now.clone()),
                ..provenance.stamp(NewMemory::new(memory_id.clone(), content, &now))
            })?;

            // The source, then every tag. This is the reason to prefer this over
            // the export route, so it is not conditional on anything.
            for (name, kind) in std::iter::once((item.source_name.as_str(), SOURCE_ENTITY_KIND))
                .chain(tags.iter().map(|t| (t.as_str(), TAG_ENTITY_KIND)))
            {
                let before = entity_exists(page, name)?;
                let entity = upsert_entity(
                    page,
                    &EntityInput {
                        name: name.to_string(),
                        kind: Some(kind.to_string()),
                        aliases: Vec::new(),
                    },
                )?;
                if !before {
                    result.entities_created += 1;
                }
                if link_memory_entity(page, &memory_id, &entity.id)? {
                    result.entity_links += 1;
                }
            }

            match prior {
                Some(prior) => {
                    // A fresh memory rather than an in-place edit, with the old one
                    // pointed at the new. Every read path filters
                    // `superseded_by IS NULL`, so the previous version drops out of
                    // search while staying in the database.
                    Memories::new(page).set_superseded_by(&prior.memory_id, &memory_id, None)?;
                    result.updated += 1;
                }
                None => result.created += 1,
            }

            ImportLedger::new(page).record_dbs(
                &item.source_name,
                &item.external_id,
                &memory_id,
                &item.content_hash,
                &now,
            )?;
        }
        Ok(())
    })?;

    result.imported = result.created + result.updated;
    Ok(result)
}

/// Whether an entity of this name is already recorded.
///
/// Checked before the upsert only so the result can report how many entities
/// are genuinely new; `upsert_entity` merges either way.
fn entity_exists(store: &Store<'_>, name: &str) -> crate::db::Result<bool> {
    Ok(crate::entity::get_entity_by_id(store, &crate::entity::entity_id(name))?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::imports::ImportLedger;
    use crate::db::Database;

    /// A `dbs`-shaped archive of `(external_id, content_hash, tags)` items,
    /// all from one source, in a scratch directory under the import root.
    fn archive(name: &str, items: &[(&str, &str, &[&str])]) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(crate::import_paths::home_dir_var().unwrap())
            .join(format!("rrm_dbs_unit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dbs.db");
        let mut sql = String::from(
            "CREATE TABLE sources (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             INSERT INTO sources (id, name) VALUES (1, 'raindrop');
             CREATE TABLE items (
                 id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL,
                 external_id TEXT NOT NULL, item_kind TEXT, title TEXT, url TEXT,
                 body TEXT, tags_json TEXT, item_created_at TEXT,
                 item_updated_at TEXT, content_hash TEXT NOT NULL,
                 deleted INTEGER NOT NULL DEFAULT 0
             );",
        );
        for (external_id, hash, tags) in items {
            let tags = serde_json::to_string(tags).unwrap().replace('\'', "''");
            sql.push_str(&format!(
                "INSERT INTO items (source_id, external_id, item_kind, title, body,
                     tags_json, item_created_at, item_updated_at, content_hash)
                 VALUES (1, '{external_id}', 'link', 'title {external_id}', 'body', '{tags}',
                     '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', '{hash}');"
            ));
        }
        crate::db::legacy_sqlite::fixture(&path, &sql).unwrap();
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

    /// An import, then a rerun where one item changed, as text, on `db`.
    fn exercise(db: &Database, first: &std::path::Path, second: &std::path::Path) -> Vec<String> {
        let store = db.store();
        let ledger = |store: &Store<'_>| {
            let mut tracked: Vec<String> = ImportLedger::new(store)
                .dbs_tracked("raindrop", &["x1", "x2", "x3"])
                .unwrap()
                .into_iter()
                .map(|(k, v)| format!("{k:?}={v:?}"))
                .collect();
            tracked.sort();
            format!(
                "{tracked:?} {:?}",
                ImportLedger::new(store).live_dbs_memories(None).unwrap()
            )
        };
        let entities = |store: &Store<'_>| {
            ["raindrop", "shared", "only-x3"]
                .map(|name| {
                    crate::entity::get_entity_by_id(store, &crate::entity::entity_id(name))
                        .unwrap()
                        .is_some()
                })
                .to_vec()
        };
        vec![
            format!("{:?}", pull_dbs(&store, &input(first)).unwrap()),
            ledger(&store),
            format!("{:?}", entities(&store)),
            format!("{:?}", pull_dbs(&store, &input(second)).unwrap()),
            ledger(&store),
            format!("{:?}", entities(&store)),
        ]
    }

    #[test]
    fn a_dbs_page_imports_and_a_rerun_updates_what_changed() {
        // Every item shares a tag, so each one after the first finds the
        // entity an earlier item in the same page created.
        let first = archive(
            "first",
            &[
                ("x1", "h1", &["shared"]),
                ("x2", "h2", &["shared"]),
                ("x3", "h3", &["shared", "only-x3"]),
            ],
        );
        let second = archive(
            "second",
            &[
                ("x1", "h1", &["shared"]),
                ("x2", "h2-changed", &["shared"]),
                ("x3", "h3", &["shared", "only-x3"]),
            ],
        );
        let db = Database::open_in_memory().unwrap();
        let seen = exercise(&db, &first, &second);
        assert!(seen[0].contains("created: 3"), "{}", seen[0]);
        assert!(seen[1].contains("x1") && seen[1].contains("x3"), "{}", seen[1]);
        assert!(seen[3].contains("updated: 1"), "{}", seen[3]);
        assert!(seen[4].contains("h2-changed"), "{}", seen[4]);
        assert_eq!(seen[2], seen[5], "a rerun creates no new entities");
        for path in [first, second] {
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }
}
