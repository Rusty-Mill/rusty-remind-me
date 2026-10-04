//! Storage for the wiki's index: `wiki_pages`, `wiki_links`, `wiki_meta` and
//! the full-text index over the pages, on the engine (`db::engine::wiki`),
//! whose index is rebuilt from the pages at open.
//!
//! The files under the wiki directory are the source of truth
//! ([`crate::wiki_fs`]); these tables are a cache of them. Every read and
//! write [`crate::wiki`] and [`crate::wiki_fs`] make goes through here. The
//! rules stay with them: slugs, reserved pages, what a reconcile re-indexes,
//! the load budget and the compile watermark.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::wiki::{WikiPage, WikiSearchHit};
use std::collections::HashMap;

/// A page's title and summary, as the generated index lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSummary {
    pub title: String,
    pub summary: String,
}

/// The wiki tables, on the engine.
pub struct WikiIndex<'c> {
    engine: &'c EngineLock,
}

impl<'c> WikiIndex<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    // --- pages -----------------------------------------------------------

    /// Store `page`, replacing every column of an existing row with its slug.
    pub fn upsert(&self, page: &WikiPage) -> Result<()> {
        engine::wiki::upsert(&mut self.engine.lock(), page)
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
        engine::wiki::upsert_unbacked(
            &mut self.engine.lock(),
            slug,
            title,
            content,
            summary,
            updated_at,
        )
    }

    /// The page with slug `slug`, if there is one.
    pub fn get(&self, slug: &str) -> Result<Option<WikiPage>> {
        Ok(engine::wiki::get(&self.engine.lock(), slug))
    }

    /// Every page, most recently updated first.
    pub fn recent_first(&self) -> Result<Vec<WikiPage>> {
        Ok(engine::wiki::recent_first(&self.engine.lock()))
    }

    /// Every page, most recently updated first, ties by title ignoring case.
    pub fn recent_first_then_title(&self) -> Result<Vec<WikiPage>> {
        Ok(engine::wiki::recent_first_then_title(&self.engine.lock()))
    }

    /// Every page's title and summary, by title ignoring case.
    pub fn summaries_by_title(&self) -> Result<Vec<PageSummary>> {
        Ok(engine::wiki::summaries_by_title(&self.engine.lock()))
    }

    /// Every page's cached file mtime, by slug.
    pub fn mtimes(&self) -> Result<HashMap<String, f64>> {
        Ok(engine::wiki::mtimes(&self.engine.lock()))
    }

    /// Remove the page `slug` and the links out of it. Returns whether a
    /// page was there.
    pub fn remove(&self, slug: &str) -> Result<bool> {
        engine::wiki::remove(&mut self.engine.lock(), slug)
    }

    // --- links -----------------------------------------------------------

    /// Replace the links out of `slug` with `links`, each a destination slug
    /// and title. Replaced wholesale, so an edit that drops a link drops the
    /// edge.
    pub fn replace_links(&self, slug: &str, links: &[(String, String)]) -> Result<()> {
        engine::wiki::replace_links(&mut self.engine.lock(), slug, links)
    }

    /// How many links leave `slug`.
    pub fn link_count(&self, slug: &str) -> Result<usize> {
        Ok(engine::wiki::link_count(&self.engine.lock(), slug))
    }

    // --- full-text search ------------------------------------------------

    /// Pages matching any of `phrases` (see [`crate::fts::query_phrases`]),
    /// best BM25 first, at most `limit`, each with a bracketed snippet of its
    /// content.
    pub fn search(&self, phrases: &[String], limit: usize) -> Result<Vec<WikiSearchHit>> {
        Ok(engine::wiki::search(&self.engine.lock(), phrases, limit))
    }

    // --- meta ------------------------------------------------------------

    /// The `wiki_meta` value under `key`, if set.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(engine::wiki::meta(&self.engine.lock(), key))
    }

    /// Set the `wiki_meta` value under `key`.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        engine::wiki::set_meta(&mut self.engine.lock(), key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

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
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let wiki = WikiIndex::new(&store);
        wiki.upsert(&page("a", 42.0, T1)).unwrap();
        wiki.upsert_unbacked("a", "A", "new", "", T2).unwrap();
        wiki.upsert_unbacked("b", "B", "body", "", T2).unwrap();

        let a = wiki.get("a").unwrap().unwrap();
        assert_eq!((a.content.as_str(), a.mtime), ("new", 42.0));
        assert_eq!(wiki.get("b").unwrap().unwrap().mtime, 0.0);
    }

    #[test]
    fn remove_takes_the_links_and_reports_a_missing_page() {
        let db = Database::open_in_memory().unwrap();
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
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let wiki = WikiIndex::new(&store);
        assert_eq!(wiki.meta("k").unwrap(), None);
        wiki.set_meta("k", "1").unwrap();
        wiki.set_meta("k", "2").unwrap();
        assert_eq!(wiki.meta("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn listings_order_by_recency_then_title_ignoring_case() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let wiki = WikiIndex::new(&store);
        let titled = |slug: &str, title: &str, at: &str| WikiPage {
            title: title.to_string(),
            ..page(slug, 1.0, at)
        };
        wiki.upsert(&titled("x", "beta", T1)).unwrap();
        wiki.upsert(&titled("y", "Alpha", T2)).unwrap();
        wiki.upsert(&titled("z", "alpha2", T2)).unwrap();

        let slugs =
            |pages: Vec<WikiPage>| -> Vec<String> { pages.into_iter().map(|p| p.slug).collect() };
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
    }

    /// The full-text index on one corpus: the matching pages, best first,
    /// with bracketed snippets, and a rewrite replacing a page's entry.
    #[test]
    fn search_finds_and_ranks_pages_and_a_rewrite_replaces_the_entry() {
        let corpus = [
            ("ownership", "Ownership", "Rust ownership rules: each value has one owner. Borrowing lends it without moving."),
            ("borrowing", "Borrowing", "Borrowing and borrowing again: shared borrows and mutable borrows."),
            ("lifetimes", "Lifetimes", "Lifetimes tie a reference to the owner it borrows from."),
            ("unrelated", "Gardening", "Tomatoes like sun, and so does borrowing light."),
        ];
        let db = Database::open_in_memory().unwrap();
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
        let hits = wiki
            .search(&crate::fts::query_phrases("borrowing owner"), 10)
            .unwrap();
        let slugs: Vec<&str> = hits.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs.len(), 3, "{slugs:?}");
        assert!(
            !slugs.contains(&"unrelated"),
            "the rewrite took 'borrowing' out"
        );
        assert_eq!(
            slugs[0], "ownership",
            "the page matching both terms ranks first"
        );
        assert!(
            hits.iter()
                .all(|h| h.snippet.contains('[') && h.snippet.contains(']')),
            "every hit brackets a matched term: {hits:?}"
        );
        assert_eq!(
            wiki.search(&crate::fts::query_phrases("nothing here"), 10)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            wiki.search(&crate::fts::query_phrases("borrowing"), 1)
                .unwrap()
                .len(),
            1
        );
    }
}
