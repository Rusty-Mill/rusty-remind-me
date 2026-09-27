//! The wiki's index on the engine (ADR-0023, phase 4h): pages, the links
//! out of them, the metadata keys, and the full-text index over the pages.
//! [`crate::db::wiki`] calls these when its store carries engine tables.
//!
//! The full-text index is derived data, kept in memory and rebuilt from the
//! page records whenever the tables open (§3a); every page write here
//! updates it in the same call, as the SQL's `write_wiki_page` updates
//! `wiki_fts`.

use super::{
    deleted, engine_error, engine_id, ensure_same_id, micros, pair_engine_id, EngineTables,
};
use crate::db::wiki::PageSummary;
use crate::db::Result;
use crate::wiki::{WikiPage, WikiSearchHit};
use rusty_multimodal_db_engine::fulltext::{FullTextIndex, Query, SnippetStyle};
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use uuid::Uuid;

/// Index marker: a page by slug, a link by its source page, a metadata row
/// by key.
pub struct ByKey;
/// Slot marker: a page's `updated_at` in µs; a link's position in its
/// page's list; unused (0) on a metadata row, which has no number.
pub struct Slot;

pub(crate) type PageTable = GenericMmapStore<PageRecord, ByKey, Slot>;
pub(crate) type LinkTable = GenericMmapStore<LinkRecord, ByKey, Slot>;
pub(crate) type MetaTable = GenericMmapStore<MetaRecord, ByKey, Slot>;
/// Title and content, keyed by slug: FTS5's `wiki_fts(title, content)`.
pub(crate) type PageSearch = FullTextIndex<String, 2>;

/// The content column, which snippets are drawn from.
const CONTENT: usize = 1;
/// FTS5's `snippet(wiki_fts, 1, '[', ']', '…', 12)`.
const SNIPPET: SnippetStyle<'static> = SnippetStyle {
    open: "[",
    close: "]",
    ellipsis: "…",
    tokens: 12,
};

/// One `wiki_pages` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageRecord {
    engine_id: Uuid,
    slug: String,
    title: String,
    content: String,
    summary: String,
    mtime: f64,
    updated_at: String,
    updated_us: i64,
}

impl PageRecord {
    fn new(page: &WikiPage) -> Self {
        Self {
            engine_id: engine_id(&page.slug),
            slug: page.slug.clone(),
            title: page.title.clone(),
            content: page.content.clone(),
            summary: page.summary.clone(),
            mtime: page.mtime,
            updated_at: page.updated_at.clone(),
            updated_us: micros(&page.updated_at),
        }
    }

    fn to_page(&self) -> WikiPage {
        WikiPage {
            slug: self.slug.clone(),
            title: self.title.clone(),
            content: self.content.clone(),
            summary: self.summary.clone(),
            mtime: self.mtime,
            updated_at: self.updated_at.clone(),
        }
    }
}

/// One `wiki_links` row, keyed by its source and destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    engine_id: Uuid,
    src_slug: String,
    dst_slug: String,
    dst_title: String,
    position: i64,
}

/// One `wiki_meta` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaRecord {
    engine_id: Uuid,
    key: String,
    value: String,
    slot: i64,
}

macro_rules! record {
    ($record:ty, $tag:literal, $key:ident, $slot:ident) => {
        impl Record for $record {
            type Id = Uuid;
            fn id(&self) -> Uuid {
                self.engine_id
            }
        }
        impl SchemaTag for $record {
            const SCHEMA_TAG: &'static str = $tag;
        }
        impl IndexedField<ByKey> for $record {
            type IndexValue = String;
            fn indexed_value(&self) -> &String {
                &self.$key
            }
        }
        impl ScannableField<Slot> for $record {
            type ScanValue = i64;
            fn scannable_value(&self) -> i64 {
                self.$slot
            }
            fn set_scannable_value(&mut self, value: i64) {
                self.$slot = value;
            }
        }
    };
}

record!(
    PageRecord,
    "rusty_remind_me::node::WikiPageRecord@1",
    slug,
    updated_us
);
record!(
    LinkRecord,
    "rusty_remind_me::node::WikiLinkRecord@1",
    src_slug,
    position
);
record!(
    MetaRecord,
    "rusty_remind_me::node::WikiMetaRecord@1",
    key,
    slot
);

/// The full-text index over `pages`, built at open.
pub(crate) fn index_pages(pages: &PageTable) -> PageSearch {
    let mut index = PageSearch::new();
    for id in pages.all_ids() {
        if let Some(page) = pages.get(id) {
            index.upsert(page.slug.clone(), [&page.title, &page.content]);
        }
    }
    index
}

// --- pages ---------------------------------------------------------------

/// Store `page`, replacing every field of the page with its slug.
pub(crate) fn upsert(tables: &mut EngineTables, page: &WikiPage) -> Result<()> {
    put_page(tables, PageRecord::new(page))
}

/// Store a page no file backs: a new page gets mtime 0, an existing one
/// keeps its mtime.
pub(crate) fn upsert_unbacked(
    tables: &mut EngineTables,
    slug: &str,
    title: &str,
    content: &str,
    summary: &str,
    updated_at: &str,
) -> Result<()> {
    let mtime = page_record(tables, slug).map_or(0.0, |p| p.mtime);
    let page = WikiPage {
        slug: slug.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        summary: summary.to_string(),
        mtime,
        updated_at: updated_at.to_string(),
    };
    put_page(tables, PageRecord::new(&page))
}

pub(crate) fn get(tables: &EngineTables, slug: &str) -> Option<WikiPage> {
    page_record(tables, slug).map(|p| p.to_page())
}

/// Every page, most recently updated first, as SQLite compares the text;
/// ties by slug, where SQLite leaves them unspecified.
pub(crate) fn recent_first(tables: &EngineTables) -> Vec<WikiPage> {
    let mut pages = all_pages(tables);
    pages.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    pages.iter().map(PageRecord::to_page).collect()
}

/// Every page, most recently updated first, ties by title ignoring ASCII
/// case (`COLLATE NOCASE`), then slug.
pub(crate) fn recent_first_then_title(tables: &EngineTables) -> Vec<WikiPage> {
    let mut pages = all_pages(tables);
    pages.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| nocase(&a.title, &b.title))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    pages.iter().map(PageRecord::to_page).collect()
}

/// Every page's title and summary, by title ignoring ASCII case, then slug.
pub(crate) fn summaries_by_title(tables: &EngineTables) -> Vec<PageSummary> {
    let mut pages = all_pages(tables);
    pages.sort_by(|a, b| nocase(&a.title, &b.title).then_with(|| a.slug.cmp(&b.slug)));
    pages
        .into_iter()
        .map(|p| PageSummary {
            title: p.title,
            summary: p.summary,
        })
        .collect()
}

pub(crate) fn mtimes(tables: &EngineTables) -> HashMap<String, f64> {
    all_pages(tables)
        .into_iter()
        .map(|p| (p.slug, p.mtime))
        .collect()
}

/// Remove the page `slug` and the links out of it. Whether a page was
/// there.
pub(crate) fn remove(tables: &mut EngineTables, slug: &str) -> Result<bool> {
    let existed = page_record(tables, slug).is_some();
    if existed {
        deleted(tables.wiki_pages.delete(engine_id(slug)))?;
        tables.wiki_search.remove(&slug.to_string());
    }
    delete_links(tables, slug)?;
    Ok(existed)
}

// --- links ---------------------------------------------------------------

/// Replace the links out of `slug` with `links`. A destination listed twice
/// keeps its first title, as `INSERT OR IGNORE` does.
pub(crate) fn replace_links(
    tables: &mut EngineTables,
    slug: &str,
    links: &[(String, String)],
) -> Result<()> {
    delete_links(tables, slug)?;
    for (position, (dst_slug, dst_title)) in links.iter().enumerate() {
        let key = pair_engine_id(slug, dst_slug);
        if tables.wiki_links.get(key).is_some() {
            continue;
        }
        let record = LinkRecord {
            engine_id: key,
            src_slug: slug.to_string(),
            dst_slug: dst_slug.clone(),
            dst_title: dst_title.clone(),
            position: i64::try_from(position).unwrap_or(i64::MAX),
        };
        tables.wiki_links.insert(record).map_err(engine_error)?;
    }
    Ok(())
}

/// How many links leave `slug`.
pub(crate) fn link_count(tables: &EngineTables, slug: &str) -> usize {
    links_of(tables, slug).len()
}

// --- full-text search ----------------------------------------------------

/// Pages matching any of `phrases`, best BM25 first, at most `limit`, each
/// with a bracketed snippet of its content.
pub(crate) fn search(
    tables: &EngineTables,
    phrases: &[String],
    limit: usize,
) -> Vec<WikiSearchHit> {
    let query = Query::any_of(phrases.iter().map(String::as_str));
    tables
        .wiki_search
        .search(&query)
        .into_iter()
        .filter_map(|hit| {
            let page = page_record(tables, &hit.key)?;
            let snippet = tables.wiki_search.snippet(
                &query,
                &hit,
                [&page.title, &page.content],
                Some(CONTENT),
                SNIPPET,
            );
            Some(WikiSearchHit {
                slug: page.slug,
                title: page.title,
                summary: page.summary,
                snippet,
            })
        })
        .take(limit)
        .collect()
}

// --- meta ----------------------------------------------------------------

pub(crate) fn meta(tables: &EngineTables, key: &str) -> Option<String> {
    tables
        .wiki_meta
        .get(engine_id(key))
        .filter(|m| m.key == key)
        .map(|m| m.value)
}

pub(crate) fn set_meta(tables: &mut EngineTables, key: &str, value: &str) -> Result<()> {
    let record = MetaRecord {
        engine_id: engine_id(key),
        key: key.to_string(),
        value: value.to_string(),
        slot: 0,
    };
    match tables.wiki_meta.get(record.engine_id) {
        Some(stored) => {
            ensure_same_id(&stored.key, key)?;
            tables.wiki_meta.replace(record).map_err(engine_error)
        }
        None => tables.wiki_meta.insert(record).map_err(engine_error),
    }
}

// --- helpers -------------------------------------------------------------

fn put_page(tables: &mut EngineTables, record: PageRecord) -> Result<()> {
    let slug = record.slug.clone();
    tables
        .wiki_search
        .upsert(slug.clone(), [&record.title, &record.content]);
    match tables.wiki_pages.get(record.engine_id) {
        Some(stored) => {
            ensure_same_id(&stored.slug, &slug)?;
            tables.wiki_pages.replace(record).map_err(engine_error)
        }
        None => tables.wiki_pages.insert(record).map_err(engine_error),
    }
}

fn page_record(tables: &EngineTables, slug: &str) -> Option<PageRecord> {
    tables
        .wiki_pages
        .get(engine_id(slug))
        .filter(|p| p.slug == slug)
}

fn all_pages(tables: &EngineTables) -> Vec<PageRecord> {
    tables
        .wiki_pages
        .all_ids()
        .into_iter()
        .filter_map(|id| tables.wiki_pages.get(id))
        .collect()
}

fn links_of(tables: &EngineTables, slug: &str) -> Vec<Uuid> {
    FilterEq::<LinkRecord, ByKey>::filter_eq(&tables.wiki_links, &slug.to_string())
}

fn delete_links(tables: &mut EngineTables, slug: &str) -> Result<()> {
    for link in links_of(tables, slug) {
        deleted(tables.wiki_links.delete(link))?;
    }
    Ok(())
}

/// SQLite's `COLLATE NOCASE`: ASCII letters fold, nothing else does.
fn nocase(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(slug: &str, content: &str) -> WikiPage {
        WikiPage {
            slug: slug.to_string(),
            title: slug.to_uppercase(),
            content: content.to_string(),
            summary: String::new(),
            mtime: 1.0,
            updated_at: "2026-09-27T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn the_search_index_is_rebuilt_at_open() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_wiki_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            upsert(&mut tables, &page("rust", "ownership and borrowing")).unwrap();
        }
        let tables = EngineTables::open(&dir).unwrap();
        let hits = search(&tables, &["borrowing".to_string()], 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "rust");
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nocase_folds_only_ascii() {
        assert_eq!(nocase("Apple", "apple"), Ordering::Equal);
        assert_eq!(nocase("apple", "Banana"), Ordering::Less);
        assert_ne!(nocase("É", "é"), Ordering::Equal);
    }
}
