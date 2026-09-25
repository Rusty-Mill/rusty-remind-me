use crate::fts::sanitize_fts_query;
use crate::wiki_import::slugify;
use chrono::Utc;
use rusqlite::{params, Connection, Result, Row};
use serde::{Deserialize, Serialize};

/// Generated system pages, refused by delete and excluded from listings.
///
/// `index.md` is regenerated on every write and `log.md` is append-only, so
/// letting a caller overwrite either by hand would put the wiki permanently at
/// odds with itself.
pub const RESERVED_SLUGS: [&str; 3] = ["index", "log", "schema"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub slug: String,
    pub title: String,
    pub content: String,
    /// One-line summary shown in the wiki index.
    pub summary: String,
    /// Source-file modification time, used by the reconcile pass. Zero for a
    /// page that has never been backed by a file.
    pub mtime: f64,
    pub updated_at: String,
}

/// Outcome of a wiki delete attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WikiDeleteOutcome {
    Deleted,
    NotFound,
    /// The slug names a reserved system page; nothing was deleted.
    Reserved,
}

const WIKI_COLUMNS: &str = "slug, title, content, summary, mtime, updated_at";

fn parse_wiki_row(row: &Row) -> Result<WikiPage> {
    Ok(WikiPage {
        slug: row.get("slug")?,
        title: row.get("title")?,
        content: row.get("content")?,
        summary: row.get("summary")?,
        mtime: row.get("mtime")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Write a row straight into the index, bypassing the filesystem.
///
/// **Not the public write path** — [`crate::wiki_fs::Wiki::write_page`] is,
/// because files are the source of truth and a row written here would be erased
/// by the next reconcile. Kept for tests that exercise the index in isolation.
#[doc(hidden)]
pub fn write_wiki_page(
    conn: &Connection,
    slug: &str,
    title: &str,
    content: &str,
    summary: &str,
) -> Result<WikiPage> {
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO wiki_pages (slug, title, content, summary, mtime, updated_at)
         VALUES (?, ?, ?, ?, 0, ?)
         ON CONFLICT(slug) DO UPDATE SET
            title = excluded.title,
            content = excluded.content,
            summary = excluded.summary,
            updated_at = excluded.updated_at",
        params![slug, title, content, summary, now],
    )?;

    get_wiki_page(conn, slug)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get_wiki_page(conn: &Connection, slug: &str) -> Result<Option<WikiPage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM wiki_pages WHERE slug = ?",
        WIKI_COLUMNS
    ))?;
    let mut rows = stmt.query_map(params![slug], parse_wiki_row)?;

    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}

pub fn list_wiki_pages(conn: &Connection) -> Result<Vec<WikiPage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM wiki_pages ORDER BY updated_at DESC",
        WIKI_COLUMNS
    ))?;
    let rows = stmt.query_map([], parse_wiki_row)?;

    let mut pages = Vec::new();
    for r in rows {
        pages.push(r?);
    }
    Ok(pages)
}

/// One hit from [`search_wiki_pages`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiSearchHit {
    pub slug: String,
    pub title: String,
    pub summary: String,
    /// Matching excerpt with the hit bracketed, from FTS5's `snippet()`.
    pub snippet: String,
}

/// Inclusive bounds the reference enforces on `WikiSearchInput`.
pub const WIKI_SEARCH_LIMIT_MIN: usize = 1;
pub const WIKI_SEARCH_LIMIT_MAX: usize = 50;
pub const WIKI_SEARCH_LIMIT_DEFAULT: usize = 10;

/// Full-text search over wiki page titles and content, ranked by BM25.
///
/// Distinct from memory search: this covers synthesised pages, not the raw
/// memory store. No vitality or RRF applies — the reference ranks wiki hits by
/// BM25 alone.
///
/// A query with no searchable tokens returns no hits rather than erroring;
/// see [`sanitize_fts_query`].
pub fn search_wiki_pages(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<WikiSearchHit>> {
    let match_expr = sanitize_fts_query(query);
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(WIKI_SEARCH_LIMIT_MIN, WIKI_SEARCH_LIMIT_MAX);

    let mut stmt = conn.prepare(
        "SELECT wp.slug, wp.title, wp.summary,
                snippet(wiki_fts, 1, '[', ']', '…', 12) AS snippet
           FROM wiki_fts
           JOIN wiki_pages wp ON wp.rowid = wiki_fts.rowid
          WHERE wiki_fts MATCH ?
          ORDER BY bm25(wiki_fts)
          LIMIT ?",
    )?;

    let rows = stmt.query_map(params![match_expr, limit as i64], |row| {
        Ok(WikiSearchHit {
            slug: row.get("slug")?,
            title: row.get("title")?,
            summary: row.get("summary")?,
            snippet: row.get("snippet")?,
        })
    })?;

    rows.collect()
}

/// Delete a wiki page addressed by either its title or its slug.
///
/// Both forms work because the input is run through [`slugify`], which is
/// idempotent on a string that is already a slug: `"VLAN Setup!"` and
/// `"vlan-setup"` both resolve to `vlan-setup`. This is exactly how the
/// reference's `wiki.delete_page` accepts either form.
///
/// Reserved system pages ([`RESERVED_SLUGS`]) are refused rather than deleted.
pub fn delete_wiki_page(conn: &Connection, title_or_slug: &str) -> Result<WikiDeleteOutcome> {
    let slug = slugify(title_or_slug);

    if RESERVED_SLUGS.contains(&slug.as_str()) {
        return Ok(WikiDeleteOutcome::Reserved);
    }

    let affected = conn.execute("DELETE FROM wiki_pages WHERE slug = ?", params![slug])?;
    Ok(if affected > 0 {
        WikiDeleteOutcome::Deleted
    } else {
        WikiDeleteOutcome::NotFound
    })
}
