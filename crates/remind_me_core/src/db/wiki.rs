//! Storage for the wiki's index: `wiki_pages`, `wiki_links`, `wiki_meta` and
//! the `wiki_fts` full-text index over the pages.
//!
//! ADR-0023 phase 1, step 3. The files under the wiki directory are the
//! source of truth ([`crate::wiki_fs`]); these tables are a cache of them.
//! Every statement [`crate::wiki`] and [`crate::wiki_fs`] ran lives here.
//! The rules stay with them: slugs, reserved pages, what a reconcile
//! re-indexes, the load budget and the compile watermark.
//!
//! With the `engine-store` feature, a store that carries engine tables keeps
//! all four there (`db::engine::wiki`, ADR-0023 phase 4h), the full-text
//! index being the engine's own, rebuilt from the pages at open.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
use crate::db::derived::write_wiki_page;
use crate::wiki::{WikiPage, WikiSearchHit};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::HashMap;

const PAGE_COLUMNS: &str = "slug, title, content, summary, mtime, updated_at";

fn parse_page_row(row: &Row) -> rusqlite::Result<WikiPage> {
    Ok(WikiPage {
        slug: row.get("slug")?,
        title: row.get("title")?,
        content: row.get("content")?,
        summary: row.get("summary")?,
        mtime: row.get("mtime")?,
        updated_at: row.get("updated_at")?,
    })
}

/// A page's title and summary, as the generated index lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSummary {
    pub title: String,
    pub summary: String,
}

/// The wiki tables, over one store.
pub struct WikiIndex<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    engine: Option<&'c Mutex<EngineTables>>,
}

impl<'c> WikiIndex<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            engine: store.engine(),
        }
    }

    // --- pages -----------------------------------------------------------

    /// Store `page`, replacing every column of an existing row with its slug.
    pub fn upsert(&self, page: &WikiPage) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::wiki::upsert(&mut engine.lock(), page);
        }
        write_wiki_page(self.conn, &page.slug, || self.upsert_row(page))?;
        Ok(())
    }

    fn upsert_row(&self, page: &WikiPage) -> Result<usize> {
        Ok(self.conn.execute(
            "INSERT INTO wiki_pages (slug, title, content, summary, mtime, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(slug) DO UPDATE SET
                title = excluded.title, content = excluded.content,
                summary = excluded.summary, mtime = excluded.mtime,
                updated_at = excluded.updated_at",
            params![
                page.slug,
                page.title,
                page.content,
                page.summary,
                page.mtime,
                page.updated_at
            ],
        )?)
    }

    /// Store a page that no file backs: a new row gets mtime 0, and an
    /// existing row keeps its mtime.
    pub fn upsert_unbacked(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        summary: &str,
        updated_at: &str,
    ) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::wiki::upsert_unbacked(
                &mut engine.lock(),
                slug,
                title,
                content,
                summary,
                updated_at,
            );
        }
        write_wiki_page(self.conn, slug, || {
            Ok(self.conn.execute(
                "INSERT INTO wiki_pages (slug, title, content, summary, mtime, updated_at)
             VALUES (?, ?, ?, ?, 0, ?)
             ON CONFLICT(slug) DO UPDATE SET
                title = excluded.title,
                content = excluded.content,
                summary = excluded.summary,
                updated_at = excluded.updated_at",
                params![slug, title, content, summary, updated_at],
            )?)
        })?;
        Ok(())
    }

    /// The page with slug `slug`, if there is one.
    pub fn get(&self, slug: &str) -> Result<Option<WikiPage>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::get(&engine.lock(), slug));
        }
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM wiki_pages WHERE slug = ?"),
                params![slug],
                parse_page_row,
            )
            .optional()?)
    }

    /// Every page, most recently updated first.
    pub fn recent_first(&self) -> Result<Vec<WikiPage>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::recent_first(&engine.lock()));
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PAGE_COLUMNS} FROM wiki_pages ORDER BY updated_at DESC"
        ))?;
        let rows = stmt
            .query_map([], parse_page_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Every page, most recently updated first, ties by title ignoring case.
    pub fn recent_first_then_title(&self) -> Result<Vec<WikiPage>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::recent_first_then_title(&engine.lock()));
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PAGE_COLUMNS} FROM wiki_pages
              ORDER BY updated_at DESC, title COLLATE NOCASE"
        ))?;
        let rows = stmt
            .query_map([], parse_page_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Every page's title and summary, by title ignoring case.
    pub fn summaries_by_title(&self) -> Result<Vec<PageSummary>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::summaries_by_title(&engine.lock()));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT title, summary FROM wiki_pages ORDER BY title COLLATE NOCASE")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PageSummary {
                    title: r.get(0)?,
                    summary: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Every page's cached file mtime, by slug.
    pub fn mtimes(&self) -> Result<HashMap<String, f64>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::mtimes(&engine.lock()));
        }
        let mut stmt = self.conn.prepare("SELECT slug, mtime FROM wiki_pages")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Remove the page `slug` and the links out of it. Returns whether a
    /// page was there.
    pub fn remove(&self, slug: &str) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::wiki::remove(&mut engine.lock(), slug);
        }
        let removed = write_wiki_page(self.conn, slug, || {
            Ok(self
                .conn
                .execute("DELETE FROM wiki_pages WHERE slug = ?", params![slug])?)
        })?;
        self.conn
            .execute("DELETE FROM wiki_links WHERE src_slug = ?", params![slug])?;
        Ok(removed > 0)
    }

    // --- links -----------------------------------------------------------

    /// Replace the links out of `slug` with `links`, each a destination slug
    /// and title. Replaced wholesale, so an edit that drops a link drops the
    /// edge.
    pub fn replace_links(&self, slug: &str, links: &[(String, String)]) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::wiki::replace_links(&mut engine.lock(), slug, links);
        }
        self.conn
            .execute("DELETE FROM wiki_links WHERE src_slug = ?", params![slug])?;
        for (dst_slug, dst_title) in links {
            self.conn.execute(
                "INSERT OR IGNORE INTO wiki_links (src_slug, dst_slug, dst_title)
                 VALUES (?, ?, ?)",
                params![slug, dst_slug, dst_title],
            )?;
        }
        Ok(())
    }

    /// How many links leave `slug`.
    pub fn link_count(&self, slug: &str) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::link_count(&engine.lock(), slug));
        }
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM wiki_links WHERE src_slug = ?",
            params![slug],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    // --- full-text search ------------------------------------------------

    /// Pages matching any of `phrases` (see [`crate::fts::query_phrases`]),
    /// best BM25 first, at most `limit`, each with a bracketed snippet of its
    /// content.
    pub fn search(&self, phrases: &[String], limit: usize) -> Result<Vec<WikiSearchHit>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::search(&engine.lock(), phrases, limit));
        }
        let match_expr = crate::fts::match_expression(phrases);
        let mut stmt = self.conn.prepare(
            "SELECT wp.slug, wp.title, wp.summary,
                    snippet(wiki_fts, 1, '[', ']', '…', 12) AS snippet
               FROM wiki_fts
               JOIN wiki_pages wp ON wp.rowid = wiki_fts.rowid
              WHERE wiki_fts MATCH ?
              ORDER BY bm25(wiki_fts)
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![match_expr, limit as i64], |row| {
                Ok(WikiSearchHit {
                    slug: row.get("slug")?,
                    title: row.get("title")?,
                    summary: row.get("summary")?,
                    snippet: row.get("snippet")?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    // --- meta ------------------------------------------------------------

    /// The `wiki_meta` value under `key`, if set.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::wiki::meta(&engine.lock(), key));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM wiki_meta WHERE key = ?",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Set the `wiki_meta` value under `key`.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::wiki::set_meta(&mut engine.lock(), key, value);
        }
        self.conn.execute(
            "INSERT INTO wiki_meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{on_each_backend, Database};

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn page(slug: &str, mtime: f64, updated_at: &str) -> WikiPage {
        WikiPage {
            slug: slug.to_string(),
            title: slug.to_uppercase(),
            content: format!("about {slug}"),
            summary: String::new(),
            mtime,
            updated_at: updated_at.to_string(),
        }
    }

    #[test]
    fn an_unbacked_write_keeps_the_cached_mtime() {
        on_each_backend(|db| {
            let store = db.store();
            let wiki = WikiIndex::new(&store);
            wiki.upsert(&page("a", 42.0, T1)).unwrap();
            wiki.upsert_unbacked("a", "A", "new", "", T2).unwrap();
            wiki.upsert_unbacked("b", "B", "body", "", T2).unwrap();

            let a = wiki.get("a").unwrap().unwrap();
            assert_eq!((a.content.as_str(), a.mtime), ("new", 42.0));
            assert_eq!(wiki.get("b").unwrap().unwrap().mtime, 0.0);
        });
    }

    #[test]
    fn remove_takes_the_links_and_reports_a_missing_page() {
        on_each_backend(|db| {
            let store = db.store();
            let wiki = WikiIndex::new(&store);
            wiki.upsert(&page("a", 1.0, T1)).unwrap();
            wiki.replace_links(
                "a",
                &[
                    ("b".to_string(), "B".to_string()),
                    ("b".to_string(), "dup".to_string()),
                    ("c".to_string(), "C".to_string()),
                ],
            )
            .unwrap();
            assert_eq!(
                wiki.link_count("a").unwrap(),
                2,
                "a duplicate link is ignored"
            );

            assert!(wiki.remove("a").unwrap());
            assert!(!wiki.remove("a").unwrap());
            assert_eq!(wiki.link_count("a").unwrap(), 0);
            assert!(wiki.get("a").unwrap().is_none());
        });
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        on_each_backend(|db| {
            let store = db.store();
            let wiki = WikiIndex::new(&store);
            assert_eq!(wiki.meta("k").unwrap(), None);
            wiki.set_meta("k", "1").unwrap();
            wiki.set_meta("k", "2").unwrap();
            assert_eq!(wiki.meta("k").unwrap().as_deref(), Some("2"));
        });
    }

    #[test]
    fn listings_order_as_the_sql_does() {
        on_each_backend(|db| {
            let store = db.store();
            let wiki = WikiIndex::new(&store);
            let titled = |slug: &str, title: &str, at: &str| WikiPage {
                title: title.to_string(),
                ..page(slug, 1.0, at)
            };
            wiki.upsert(&titled("x", "beta", T1)).unwrap();
            wiki.upsert(&titled("y", "Alpha", T2)).unwrap();
            wiki.upsert(&titled("z", "alpha2", T2)).unwrap();

            let slugs = |pages: Vec<WikiPage>| -> Vec<String> {
                pages.into_iter().map(|p| p.slug).collect()
            };
            assert_eq!(
                slugs(wiki.recent_first_then_title().unwrap()),
                ["y", "z", "x"]
            );
            assert_eq!(slugs(wiki.recent_first().unwrap())[2], "x");
            let titles: Vec<String> = wiki
                .summaries_by_title()
                .unwrap()
                .into_iter()
                .map(|s| s.title)
                .collect();
            assert_eq!(titles, ["Alpha", "alpha2", "beta"], "case-insensitive");
            assert_eq!(wiki.mtimes().unwrap().len(), 3);
        });
    }

    /// The engine's full-text index against FTS5 on one corpus: the same
    /// pages, in the same order, with the same snippets.
    #[test]
    fn search_finds_and_ranks_as_fts5_does() {
        let corpus = [
            ("ownership", "Ownership", "Rust ownership rules: each value has one owner. Borrowing lends it without moving."),
            ("borrowing", "Borrowing", "Borrowing and borrowing again: shared borrows and mutable borrows."),
            ("lifetimes", "Lifetimes", "Lifetimes tie a reference to the owner it borrows from."),
            ("unrelated", "Gardening", "Tomatoes like sun."),
        ];
        let run = |db: &Database| -> Vec<WikiSearchHit> {
            let store = db.store();
            let wiki = WikiIndex::new(&store);
            for (slug, title, content) in corpus {
                wiki.upsert(&WikiPage {
                    slug: slug.to_string(),
                    title: title.to_string(),
                    content: content.to_string(),
                    summary: String::new(),
                    mtime: 1.0,
                    updated_at: T1.to_string(),
                })
                .unwrap();
            }
            // A rewrite must replace, not add to, the page's index entry.
            wiki.upsert_unbacked("unrelated", "Gardening", "Tomatoes like sun.", "", T2)
                .unwrap();
            wiki.search(&crate::fts::query_phrases("borrowing owner"), 10)
                .unwrap()
        };
        let sqlite = run(&Database::open_sqlite_in_memory().unwrap());
        let slugs: Vec<&str> = sqlite.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs.len(), 3, "{slugs:?}");
        assert!(!slugs.contains(&"unrelated"));
        #[cfg(feature = "engine-store")]
        assert_eq!(run(&Database::open_in_memory_on_engine().unwrap()), sqlite);
    }
}
