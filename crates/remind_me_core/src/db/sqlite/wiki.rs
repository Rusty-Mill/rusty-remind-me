//! Wiki pages, links and meta on SQLite: the [`WikiStore`] adapter, with
//! an FTS5 index over (title, content) kept in step by each page write.

use super::{exec, SqliteTables};
use crate::db::wiki::{PageSummary, WikiStore};
use crate::db::Result;
use crate::wiki::{WikiPage, WikiSearchHit};
use rusqlite::{params, Connection, Row};
use std::collections::HashMap;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS wiki_pages (
    slug TEXT PRIMARY KEY, title TEXT NOT NULL, content TEXT NOT NULL, summary TEXT NOT NULL,
    mtime REAL NOT NULL, updated_at TEXT NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS wiki_fts USING fts5 (slug UNINDEXED, title, content);
CREATE TABLE IF NOT EXISTS wiki_links (
    src_slug TEXT NOT NULL, dst_slug TEXT NOT NULL, dst_title TEXT NOT NULL,
    position INTEGER NOT NULL, PRIMARY KEY (src_slug, dst_slug)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS wiki_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

const PAGE: &str = "slug, title, content, summary, mtime, updated_at";

fn page(r: &Row<'_>) -> rusqlite::Result<WikiPage> {
    Ok(WikiPage {
        slug: r.get(0)?,
        title: r.get(1)?,
        content: r.get(2)?,
        summary: r.get(3)?,
        mtime: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

fn put_page(c: &Connection, p: &WikiPage) -> Result<()> {
    exec(c, "DELETE FROM wiki_fts WHERE slug = ?1", [&p.slug])?;
    exec(
        c,
        &format!("INSERT OR REPLACE INTO wiki_pages ({PAGE}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
        params![p.slug, p.title, p.content, p.summary, p.mtime, p.updated_at],
    )?;
    exec(
        c,
        "INSERT INTO wiki_fts (slug, title, content) VALUES (?1, ?2, ?3)",
        params![p.slug, p.title, p.content],
    )
    .map(drop)
}

/// An FTS5 query matching any of `phrases`.
fn any_of(phrases: &[String]) -> String {
    phrases
        .iter()
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

impl SqliteTables {
    fn pages(&self, order: &str) -> Result<Vec<WikiPage>> {
        self.all(
            &format!("SELECT {PAGE} FROM wiki_pages ORDER BY {order}"),
            [],
            page,
        )
    }

    /// Rebuild the wiki's full-text index from its pages.
    pub(super) fn rebuild_wiki_index(&mut self) -> Result<()> {
        self.atomic(|c| {
            exec(c, "DELETE FROM wiki_fts", [])?;
            exec(c, "INSERT INTO wiki_fts (slug, title, content) SELECT slug, title, content FROM wiki_pages", [])
                .map(drop)
        })
    }
}

impl WikiStore for SqliteTables {
    fn upsert(&mut self, p: &WikiPage) -> Result<()> {
        self.atomic(|c| put_page(c, p))
    }

    fn upsert_unbacked(
        &mut self,
        slug: &str,
        title: &str,
        content: &str,
        summary: &str,
        updated_at: &str,
    ) -> Result<()> {
        let mtime = self.get(slug)?.map_or(0.0, |p| p.mtime);
        let p = WikiPage {
            slug: slug.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            summary: summary.to_string(),
            mtime,
            updated_at: updated_at.to_string(),
        };
        self.atomic(|c| put_page(c, &p))
    }

    fn get(&self, slug: &str) -> Result<Option<WikiPage>> {
        self.one(
            &format!("SELECT {PAGE} FROM wiki_pages WHERE slug = ?1"),
            [slug],
            page,
        )
    }

    fn recent_first(&self) -> Result<Vec<WikiPage>> {
        self.pages("updated_at DESC, slug")
    }

    fn recent_first_then_title(&self) -> Result<Vec<WikiPage>> {
        self.pages("updated_at DESC, title COLLATE NOCASE, slug")
    }

    fn summaries_by_title(&self) -> Result<Vec<PageSummary>> {
        self.all(
            "SELECT title, summary FROM wiki_pages ORDER BY title COLLATE NOCASE, slug",
            [],
            |r| {
                Ok(PageSummary {
                    title: r.get(0)?,
                    summary: r.get(1)?,
                })
            },
        )
    }

    fn mtimes(&self) -> Result<HashMap<String, f64>> {
        Ok(self
            .all("SELECT slug, mtime FROM wiki_pages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .into_iter()
            .collect())
    }

    fn remove(&mut self, slug: &str) -> Result<bool> {
        self.atomic(|c| {
            exec(c, "DELETE FROM wiki_fts WHERE slug = ?1", [slug])?;
            exec(c, "DELETE FROM wiki_links WHERE src_slug = ?1", [slug])?;
            Ok(exec(c, "DELETE FROM wiki_pages WHERE slug = ?1", [slug])? > 0)
        })
    }

    fn replace_links(&mut self, slug: &str, links: &[(String, String)]) -> Result<()> {
        self.atomic(|c| {
            exec(c, "DELETE FROM wiki_links WHERE src_slug = ?1", [slug])?;
            for (position, (dst, title)) in links.iter().enumerate() {
                let position = i64::try_from(position).unwrap_or(i64::MAX);
                exec(
                    c,
                    "INSERT OR IGNORE INTO wiki_links VALUES (?1, ?2, ?3, ?4)",
                    params![slug, dst, title, position],
                )?;
            }
            Ok(())
        })
    }

    fn link_count(&self, slug: &str) -> Result<usize> {
        let n: i64 = self
            .one(
                "SELECT count(*) FROM wiki_links WHERE src_slug = ?1",
                [slug],
                |r| r.get(0),
            )?
            .unwrap_or(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn search(&self, phrases: &[String], n: usize) -> Result<Vec<WikiSearchHit>> {
        if phrases.is_empty() {
            return Ok(Vec::new());
        }
        self.all(
            "SELECT p.slug, p.title, p.summary, snippet(wiki_fts, 2, '[', ']', '…', 12) \
             FROM wiki_fts JOIN wiki_pages p ON p.slug = wiki_fts.slug \
             WHERE wiki_fts MATCH ?1 ORDER BY bm25(wiki_fts), p.slug LIMIT ?2",
            params![any_of(phrases), super::stats::limit(n)],
            |r| {
                Ok(WikiSearchHit {
                    slug: r.get(0)?,
                    title: r.get(1)?,
                    summary: r.get(2)?,
                    snippet: r.get(3)?,
                })
            },
        )
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        self.one("SELECT value FROM wiki_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO wiki_meta VALUES (?1, ?2)",
            [key, value],
        )
        .map(drop)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::wiki::wiki_conformance!(SqliteTables::open_in_memory().unwrap());
}
