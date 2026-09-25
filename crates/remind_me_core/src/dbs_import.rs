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

use crate::entity::{link_memory_entity, upsert_entity};
use crate::import_paths::{validate_import_database, ImportPathError};
use crate::models::{DbsImportInput, EntityInput, DBS_IMPORT_LIMIT_MAX, DBS_IMPORT_LIMIT_MIN};
use chrono::Utc;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, Result};
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
    Sqlite(rusqlite::Error),
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
            Self::Sqlite(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for DbsImportError {}

impl From<ImportPathError> for DbsImportError {
    fn from(e: ImportPathError) -> Self {
        Self::Path(e)
    }
}

impl From<rusqlite::Error> for DbsImportError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
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

/// What a previous import recorded for one item.
struct Tracked {
    memory_id: String,
    content_hash: String,
}

/// Open a `dbs` archive read-only.
///
/// `rusqlite` does not touch the file until a statement runs, so this issues
/// one immediately: a caller that passes a JPEG should learn that here, not
/// several layers into the import.
fn open_dbs(path: &Path) -> std::result::Result<Connection, DbsImportError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| DbsImportError::NotADatabase {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;

    conn.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
        .or_else(|e| match e {
            // An empty database is a valid one; it just has no tables yet, and
            // the missing-tables check below gives a better message for it.
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(DbsImportError::NotADatabase {
                path: path.display().to_string(),
                detail: other.to_string(),
            }),
        })?;

    Ok(conn)
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
    dbs: &Connection,
    input: &DbsImportInput,
    limit: usize,
) -> std::result::Result<Vec<DbsItem>, DbsImportError> {
    let mut where_sql = String::from("i.deleted = 0");
    let mut binds: Vec<String> = Vec::new();
    if !input.source.is_empty() {
        where_sql.push_str(" AND s.name = ?");
        binds.push(input.source.clone());
    }
    if !input.item_type.is_empty() {
        where_sql.push_str(" AND i.item_kind = ?");
        binds.push(input.item_type.clone());
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

    let mut statement = dbs
        .prepare(&sql)
        .map_err(|e| DbsImportError::NotADbsArchive {
            path: String::new(),
            detail: e.to_string(),
        })?;

    let mut values: Vec<rusqlite::types::Value> = binds
        .into_iter()
        .map(rusqlite::types::Value::Text)
        .collect();
    values.push(rusqlite::types::Value::Integer(limit as i64));
    values.push(rusqlite::types::Value::Integer(input.offset as i64));

    let rows = statement.query_map(params_from_iter(values), |row| {
        Ok(DbsItem {
            external_id: row.get("external_id")?,
            item_kind: row.get("item_kind")?,
            title: row.get("title")?,
            url: row.get("url")?,
            body: row.get("body")?,
            tags_json: row.get("tags_json")?,
            item_created_at: row.get("item_created_at")?,
            content_hash: row.get("content_hash")?,
            source_name: row.get("source_name")?,
        })
    })?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

/// What previous imports recorded for the items on this page.
fn tracked_state(
    conn: &Connection,
    items: &[DbsItem],
) -> Result<HashMap<(String, String), Tracked>> {
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
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT dbs_source, external_id, memory_id, content_hash
                   FROM dbs_imports
                  WHERE dbs_source = ? AND external_id IN ({})",
                placeholders
            );
            let mut values = vec![rusqlite::types::Value::Text(source.to_string())];
            values.extend(
                batch
                    .iter()
                    .map(|id| rusqlite::types::Value::Text((*id).to_string())),
            );

            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(values), |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    Tracked {
                        memory_id: row.get(2)?,
                        content_hash: row.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (key, value) = row?;
                tracked.insert(key, value);
            }
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
    conn: &Connection,
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
    let tracked = tracked_state(conn, &items)?;

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
    let tx = conn.unchecked_transaction()?;

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

        tx.execute(
            "INSERT OR IGNORE INTO memories
                (id, content, category, tags, source, metadata, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id,
                content,
                item.item_kind
                    .as_deref()
                    .filter(|k| !k.is_empty())
                    .unwrap_or(DEFAULT_CATEGORY),
                serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string()),
                format!("dbs:{}", item.source_name),
                metadata.to_string(),
                // The item's own creation time, so a memory ages from when the
                // thing happened rather than from when it was imported —
                // vitality decay reads this column.
                item.item_created_at.as_deref().unwrap_or(&now),
                now,
            ],
        )?;

        // The source, then every tag. This is the reason to prefer this over
        // the export route, so it is not conditional on anything.
        for (name, kind) in std::iter::once((item.source_name.as_str(), SOURCE_ENTITY_KIND))
            .chain(tags.iter().map(|t| (t.as_str(), TAG_ENTITY_KIND)))
        {
            let before = entity_exists(&tx, name)?;
            let entity = upsert_entity(
                &tx,
                &EntityInput {
                    name: name.to_string(),
                    kind: Some(kind.to_string()),
                    aliases: Vec::new(),
                },
            )?;
            if !before {
                result.entities_created += 1;
            }
            if link_memory_entity(&tx, &memory_id, &entity.id)? {
                result.entity_links += 1;
            }
        }

        match prior {
            Some(prior) => {
                // A fresh memory rather than an in-place edit, with the old one
                // pointed at the new. Every read path filters
                // `superseded_by IS NULL`, so the previous version drops out of
                // search while staying in the database.
                tx.execute(
                    "UPDATE memories SET superseded_by = ? WHERE id = ?",
                    params![memory_id, prior.memory_id],
                )?;
                result.updated += 1;
            }
            None => result.created += 1,
        }

        tx.execute(
            "INSERT INTO dbs_imports (dbs_source, external_id, memory_id, content_hash, imported_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(dbs_source, external_id)
             DO UPDATE SET memory_id = excluded.memory_id,
                           content_hash = excluded.content_hash,
                           imported_at = excluded.imported_at",
            params![
                item.source_name,
                item.external_id,
                memory_id,
                item.content_hash,
                now
            ],
        )?;
    }

    tx.commit()?;

    result.imported = result.created + result.updated;
    Ok(result)
}

/// Whether an entity of this name is already recorded.
///
/// Checked before the upsert only so the result can report how many entities
/// are genuinely new; `upsert_entity` merges either way.
fn entity_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(crate::entity::get_entity_by_id(conn, &crate::entity::entity_id(name))?.is_some())
}
