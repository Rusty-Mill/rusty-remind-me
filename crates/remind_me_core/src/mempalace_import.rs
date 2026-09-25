//! Bulk import from a MemPalace ChromaDB store.
//!
//! MemPalace exposes drawers one at a time through its own MCP tools, which
//! does not scale to a wing holding tens of thousands of them. This reads its
//! persistent ChromaDB store directly, read-only, the same way `dbs_import`
//! reads a foreign SQLite schema.
//!
//! # Never the vector segment
//!
//! A Chroma collection is really two segments: a `VECTOR` one (an HNSW index
//! in its own binary files) and a `METADATA` one (an ordinary SQLite table).
//! This reads only the metadata segment — the document text and any
//! wing/room tags a drawer carries — and never opens the vector segment at
//! all. That is not an optimization; it is the reason this is tractable
//! without a ChromaDB client. See `docs/adr/0001-mempalace-chroma-sqlite-read.md`
//! for what was actually verified about Chroma's on-disk schema (checked
//! against `chromadb` 0.5.0 through 1.5.9) before deciding this was safe to
//! build at all.
//!
//! # Round-tripping
//!
//! A drawer whose text carries `remind_me`'s own memory frontmatter — because
//! it arrived in MemPalace via a prior `remind_me` export — has its
//! `category`/`tags`/`created` restored from that frontmatter. Its `source`
//! is restored too, but prefixed `mempalace:`, and its memory `id` is freshly
//! minted rather than reused: that is what the reference itself does
//! (`fields["id"]` is parsed and then never read again), not an
//! embellishment on top of it. Everything else becomes one opaque memory per
//! drawer, tagged with its wing and room. MemPalace's AAAK dialect is
//! designed to be read as-is, so there is no decoding step for opaque
//! content.
//!
//! # Dedup
//!
//! Keyed on `drawer_id` alone, tracked in `mempalace_imports`. Unlike
//! `dbs_import`, there is no edit-detection: once a drawer is imported it
//! stays imported, matching the reference exactly (it has no content-hash
//! column to compare against).

use crate::models::{MempalaceImportInput, MEMPALACE_IMPORT_LIMIT_MAX, MEMPALACE_IMPORT_LIMIT_MIN};
use chrono::Utc;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Reads `REMIND_ME_MEMPALACE_PATH`, the persist-directory MemPalace's
/// ChromaDB client was opened against — not a per-call argument, since this
/// is operator configuration (see the ADR).
pub const MEMPALACE_PATH_ENV: &str = "REMIND_ME_MEMPALACE_PATH";
const DEFAULT_MEMPALACE_PATH: &str = "~/.mempalace/palace";

/// The collection MemPalace stores drawers under.
pub const COLLECTION_NAME: &str = "mempalace_drawers";
/// `source` for a drawer with no restorable frontmatter.
pub const OPAQUE_SOURCE: &str = "mempalace_import";
/// `category` fallback when neither the frontmatter nor the caller supplies one.
pub const DEFAULT_CATEGORY: &str = "mempalace_import";

const RESERVED_DOCUMENT_KEY: &str = "chroma:document";

fn expand_home(raw: &str) -> String {
    match (raw.strip_prefix("~/"), crate::import_paths::home_dir_var()) {
        (Some(rest), Ok(home)) => format!("{}/{}", home.trim_end_matches('/'), rest),
        _ => raw.to_string(),
    }
}

/// The configured MemPalace persist directory (not yet checked for existence).
pub fn mempalace_path() -> PathBuf {
    let raw =
        std::env::var(MEMPALACE_PATH_ENV).unwrap_or_else(|_| DEFAULT_MEMPALACE_PATH.to_string());
    PathBuf::from(expand_home(raw.trim()))
}

/// Why an import could not run.
#[derive(Debug)]
pub enum MempalaceImportError {
    /// No store at the configured path.
    NotFound {
        path: String,
    },
    /// A file exists there, but is not a readable SQLite database.
    NotADatabase {
        path: String,
        detail: String,
    },
    /// A readable database, but no `mempalace_drawers` collection in it —
    /// wrong path, an empty palace, or not a Chroma store at all.
    NoCollection {
        path: String,
    },
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for MempalaceImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path } => write!(f, "No MemPalace store found at {}", path),
            Self::NotADatabase { path, detail } => {
                write!(f, "Not a readable SQLite database: {} ({})", path, detail)
            }
            Self::NoCollection { path } => write!(
                f,
                "No '{}' collection in the MemPalace store at {}",
                COLLECTION_NAME, path
            ),
            Self::Sqlite(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for MempalaceImportError {}

impl From<rusqlite::Error> for MempalaceImportError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// What one page of an import did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MempalaceImportResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Drawers this page read, after the wing/room filter and before dedup.
    pub fetched: usize,
    pub already_imported: usize,
    pub to_import: usize,
    /// Of `to_import`, how many carried restorable `remind_me` frontmatter.
    pub native_format: usize,
    /// Of `to_import`, how many became one opaque memory.
    pub opaque_format: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub imported: usize,
}

/// One drawer read from the metadata segment.
struct Drawer {
    drawer_id: String,
    document: String,
    wing: Option<String>,
    room: Option<String>,
}

/// Open a Chroma store's SQLite file read-only, and confirm it is at least a
/// readable database — real content validation happens when the caller looks
/// for the collection.
fn open_store(path: &Path) -> std::result::Result<Connection, MempalaceImportError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| MempalaceImportError::NotADatabase {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;

    conn.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(MempalaceImportError::NotADatabase {
                path: path.display().to_string(),
                detail: other.to_string(),
            }),
        })?;

    Ok(conn)
}

/// The metadata segment id backing the `mempalace_drawers` collection.
fn metadata_segment_id(
    chroma: &Connection,
    store_path: &Path,
) -> std::result::Result<String, MempalaceImportError> {
    let collection_id: String = chroma
        .query_row(
            "SELECT id FROM collections WHERE name = ?",
            params![COLLECTION_NAME],
            |row| row.get(0),
        )
        .map_err(|_| MempalaceImportError::NoCollection {
            path: store_path.display().to_string(),
        })?;

    chroma
        .query_row(
            "SELECT id FROM segments WHERE collection = ? AND scope = 'METADATA'",
            params![collection_id],
            |row| row.get(0),
        )
        .map_err(|_| MempalaceImportError::NoCollection {
            path: store_path.display().to_string(),
        })
}

/// Every drawer in the metadata segment, in insertion order.
///
/// One query per drawer for its metadata rows rather than a single join: the
/// number of metadata keys per drawer is small and fixed (`chroma:document`,
/// `wing`, `room`), so the simplicity is worth more than the extra
/// round-trips.
fn read_drawers(chroma: &Connection, segment_id: &str) -> Result<Vec<Drawer>> {
    let mut stmt = chroma
        .prepare("SELECT id, embedding_id FROM embeddings WHERE segment_id = ? ORDER BY id")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![segment_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_>>()?;
    drop(stmt);

    let mut meta_stmt = chroma.prepare(
        "SELECT key, string_value FROM embedding_metadata WHERE id = ? AND key IN (?, 'wing', 'room')",
    )?;

    let mut drawers = Vec::with_capacity(rows.len());
    for (embeddings_id, drawer_id) in rows {
        let mut document = String::new();
        let mut wing = None;
        let mut room = None;
        let entries = meta_stmt
            .query_map(params![embeddings_id, RESERVED_DOCUMENT_KEY], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
        for entry in entries {
            let (key, value) = entry?;
            match key.as_str() {
                RESERVED_DOCUMENT_KEY => document = value.unwrap_or_default(),
                "wing" => wing = value,
                "room" => room = value,
                _ => {}
            }
        }
        drawers.push(Drawer {
            drawer_id,
            document,
            wing,
            room,
        });
    }
    Ok(drawers)
}

/// Split `remind_me`-native frontmatter from a drawer's content, if present.
///
/// Matches the reference's `_FRONTMATTER_RE`: one or more `key: value` lines
/// (keys are letters and underscores only) between a leading `---` and a
/// closing `---`, an optional blank line, then the body verbatim to the end
/// of the string. Written by hand rather than with a regex crate — the
/// pattern is a small, fixed grammar and this crate has no other need for
/// regular expressions.
pub fn parse_frontmatter(
    content: &str,
) -> Option<(std::collections::HashMap<String, String>, String)> {
    let mut cursor = content.strip_prefix("---\n")?;
    let mut fields = std::collections::HashMap::new();

    loop {
        if let Some(after_delimiter) = cursor.strip_prefix("---\n") {
            if fields.is_empty() {
                return None;
            }
            let body = after_delimiter
                .strip_prefix('\n')
                .unwrap_or(after_delimiter);
            return Some((fields, body.to_string()));
        }

        let line_end = cursor.find('\n')?;
        let line = &cursor[..line_end];
        let (key, value) = line.split_once(':')?;
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        fields.insert(key.trim().to_string(), value.trim().to_string());
        cursor = &cursor[line_end + 1..];
    }
}

/// Pull a page of MemPalace drawers into memory.
///
/// Reruns are safe: a drawer already recorded in `mempalace_imports` is
/// skipped. `limit` is clamped to
/// [`MEMPALACE_IMPORT_LIMIT_MIN`]..=[`MEMPALACE_IMPORT_LIMIT_MAX`] rather than
/// rejected, matching every other bounded import in this crate.
///
/// The wing/room filter and the paging are both applied here, in Rust, over
/// every drawer in the collection — the reference pushes both into Chroma's
/// own query planner (`collection.get(where=..., limit=, offset=)`), but nothing
/// about reading a flat key/value table requires that, and doing it here avoids
/// depending on whatever internal query API a given Chroma version exposes.
pub fn pull_mempalace(
    conn: &Connection,
    input: &MempalaceImportInput,
) -> std::result::Result<MempalaceImportResult, MempalaceImportError> {
    let store_dir = mempalace_path();
    let store_path = store_dir.join("chroma.sqlite3");
    if !store_path.exists() {
        return Err(MempalaceImportError::NotFound {
            path: store_path.display().to_string(),
        });
    }
    let limit = input
        .limit
        .clamp(MEMPALACE_IMPORT_LIMIT_MIN, MEMPALACE_IMPORT_LIMIT_MAX);

    let all_drawers = {
        let chroma = open_store(&store_path)?;
        let segment_id = metadata_segment_id(&chroma, &store_path)?;
        read_drawers(&chroma, &segment_id)?
        // The Chroma connection closes here, before anything is written.
    };

    let filtered: Vec<&Drawer> = all_drawers
        .iter()
        .filter(|d| input.wing.is_empty() || d.wing.as_deref() == Some(input.wing.as_str()))
        .filter(|d| input.room.is_empty() || d.room.as_deref() == Some(input.room.as_str()))
        .collect();

    let page: Vec<&Drawer> = filtered
        .iter()
        .skip(input.offset)
        .take(limit)
        .copied()
        .collect();
    let fetched = page.len();

    let mut result = MempalaceImportResult {
        wing: Some(input.wing.clone()).filter(|s| !s.is_empty()),
        room: Some(input.room.clone()).filter(|s| !s.is_empty()),
        fetched,
        offset: input.offset,
        limit,
        has_more: fetched == limit,
        ..Default::default()
    };
    if fetched == 0 {
        return Ok(result);
    }

    let already: std::collections::HashSet<String> = {
        let placeholders = std::iter::repeat_n("?", page.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT drawer_id FROM mempalace_imports WHERE drawer_id IN ({})",
            placeholders
        );
        let ids: Vec<&str> = page.iter().map(|d| d.drawer_id.as_str()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), |row| row.get::<_, String>(0))?;
        rows.collect::<Result<_>>()?
    };

    let to_import: Vec<&Drawer> = page
        .into_iter()
        .filter(|d| !already.contains(&d.drawer_id))
        .collect();

    result.already_imported = fetched - to_import.len();
    result.to_import = to_import.len();
    result.native_format = to_import
        .iter()
        .filter(|d| parse_frontmatter(&d.document).is_some())
        .count();
    result.opaque_format = to_import.len() - result.native_format;

    if input.dry_run {
        return Ok(result);
    }

    let now = Utc::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;

    for drawer in &to_import {
        let wing_val = drawer.wing.clone().unwrap_or_default();
        let room_val = drawer.room.clone().unwrap_or_default();

        let (mem_category, mem_tags, mem_source, created_at, content) =
            match parse_frontmatter(&drawer.document) {
                Some((fields, body)) => {
                    let category = fields
                        .get("category")
                        .filter(|c| !c.is_empty())
                        .cloned()
                        .or_else(|| Some(input.category.clone()).filter(|c| !c.is_empty()))
                        .unwrap_or_else(|| DEFAULT_CATEGORY.to_string());
                    let native_tags: Vec<String> = fields
                        .get("tags")
                        .map(|t| {
                            t.split(',')
                                .map(str::trim)
                                .filter(|t| !t.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let tags: Vec<String> = native_tags
                        .into_iter()
                        .chain(input.tags.iter().cloned())
                        .collect();
                    let source = format!(
                        "mempalace:{}",
                        fields
                            .get("source")
                            .map(String::as_str)
                            .unwrap_or("unknown")
                    );
                    let created = fields
                        .get("created")
                        .cloned()
                        .unwrap_or_else(|| now.clone());
                    (category, tags, source, created, body)
                }
                None => {
                    let category = if input.category.is_empty() {
                        DEFAULT_CATEGORY.to_string()
                    } else {
                        input.category.clone()
                    };
                    let tags: Vec<String> = [wing_val.as_str(), room_val.as_str()]
                        .into_iter()
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .chain(input.tags.iter().cloned())
                        .collect();
                    (
                        category,
                        tags,
                        OPAQUE_SOURCE.to_string(),
                        now.clone(),
                        drawer.document.clone(),
                    )
                }
            };

        let memory_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let metadata = serde_json::json!({
            "mempalace_drawer_id": drawer.drawer_id,
            "wing": wing_val,
            "room": room_val,
        });

        tx.execute(
            "INSERT OR IGNORE INTO memories
                (id, content, category, tags, source, metadata, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id,
                content,
                mem_category,
                serde_json::to_string(&mem_tags).unwrap_or_else(|_| "[]".to_string()),
                mem_source,
                metadata.to_string(),
                created_at,
                now,
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO mempalace_imports (drawer_id, memory_id, imported_at)
             VALUES (?, ?, ?)",
            params![drawer.drawer_id, memory_id, now],
        )?;
    }

    tx.commit()?;
    result.imported = to_import.len();
    Ok(result)
}
