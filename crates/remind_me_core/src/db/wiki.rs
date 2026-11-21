//! Storage for the wiki's index: `wiki_pages`, `wiki_links`, `wiki_meta` and
//! the full-text index over the pages, on the engine (`db::engine::wiki`),
//! whose index is rebuilt from the pages at open.
//!
//! The files under the wiki directory are the source of truth
//! ([`crate::wiki_fs`]); these tables are a cache of them. Every read and
//! write [`crate::wiki`] and [`crate::wiki_fs`] make goes through here. The
//! rules stay with them: slugs, reserved pages, what a reconcile re-indexes,
//! the load budget and the compile watermark.
//!
//! [`WikiStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::wiki`. [`WikiIndex`] is what callers take.

use super::backend::BackendLock;
use super::{Result, Store};
use crate::wiki::{WikiPage, WikiSearchHit};
use std::collections::HashMap;

/// A page's title and summary, as the generated index lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSummary {
    pub title: String,
    pub summary: String,
}

/// The wiki tables and their full-text index, as a backend stores them
/// (ADR-0035). [`conformance`] holds the behaviour every implementation
/// must show.
pub trait WikiStore {
    /// Store `page`, replacing every column of an existing row with its slug.
    fn upsert(&mut self, page: &WikiPage) -> Result<()>;

    /// Store a page that no file backs: a new row gets mtime 0, and an
    /// existing row keeps its mtime.
    fn upsert_unbacked(
        &mut self,
        slug: &str,
        title: &str,
        content: &str,
        summary: &str,
        updated_at: &str,
    ) -> Result<()>;

    /// The page with slug `slug`, if there is one.
    fn get(&self, slug: &str) -> Result<Option<WikiPage>>;

    /// Every page, most recently updated first.
    fn recent_first(&self) -> Result<Vec<WikiPage>>;

    /// Every page, most recently updated first, ties by title ignoring case.
    fn recent_first_then_title(&self) -> Result<Vec<WikiPage>>;

    /// Every page's title and summary, by title ignoring case.
    fn summaries_by_title(&self) -> Result<Vec<PageSummary>>;

    /// Every page's cached file mtime, by slug.
    fn mtimes(&self) -> Result<HashMap<String, f64>>;

    /// Remove the page `slug` and the links out of it. Returns whether a
    /// page was there.
    fn remove(&mut self, slug: &str) -> Result<bool>;

    /// Replace the links out of `slug` with `links`, each a destination slug
    /// and title. Replaced wholesale, so an edit that drops a link drops the
    /// edge; a destination listed twice keeps its first title.
    fn replace_links(&mut self, slug: &str, links: &[(String, String)]) -> Result<()>;

    /// How many links leave `slug`.
    fn link_count(&self, slug: &str) -> Result<usize>;

    /// Pages matching any of `phrases` (see [`crate::fts::query_phrases`]),
    /// best BM25 first, at most `limit`, each with a bracketed snippet of its
    /// content.
    fn search(&self, phrases: &[String], limit: usize) -> Result<Vec<WikiSearchHit>>;

    /// The `wiki_meta` value under `key`, if set.
    fn meta(&self, key: &str) -> Result<Option<String>>;

    /// Set the `wiki_meta` value under `key`.
    fn set_meta(&mut self, key: &str, value: &str) -> Result<()>;
}

/// The wiki tables of a [`Store`], one locked call at a time.
pub struct WikiIndex<'c> {
    backend: &'c BackendLock,
}

impl<'c> WikiIndex<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`WikiStore::upsert`].
    pub fn upsert(&self, page: &WikiPage) -> Result<()> {
        WikiStore::upsert(&mut *self.backend.lock()?, page)
    }

    /// See [`WikiStore::upsert_unbacked`].
    pub fn upsert_unbacked(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        summary: &str,
        updated_at: &str,
    ) -> Result<()> {
        WikiStore::upsert_unbacked(
            &mut *self.backend.lock()?,
            slug,
            title,
            content,
            summary,
            updated_at,
        )
    }

    /// See [`WikiStore::get`].
    pub fn get(&self, slug: &str) -> Result<Option<WikiPage>> {
        WikiStore::get(&*self.backend.lock()?, slug)
    }

    /// See [`WikiStore::recent_first`].
    pub fn recent_first(&self) -> Result<Vec<WikiPage>> {
        WikiStore::recent_first(&*self.backend.lock()?)
    }

    /// See [`WikiStore::recent_first_then_title`].
    pub fn recent_first_then_title(&self) -> Result<Vec<WikiPage>> {
        WikiStore::recent_first_then_title(&*self.backend.lock()?)
    }

    /// See [`WikiStore::summaries_by_title`].
    pub fn summaries_by_title(&self) -> Result<Vec<PageSummary>> {
        WikiStore::summaries_by_title(&*self.backend.lock()?)
    }

    /// See [`WikiStore::mtimes`].
    pub fn mtimes(&self) -> Result<HashMap<String, f64>> {
        WikiStore::mtimes(&*self.backend.lock()?)
    }

    /// See [`WikiStore::remove`].
    pub fn remove(&self, slug: &str) -> Result<bool> {
        WikiStore::remove(&mut *self.backend.lock()?, slug)
    }

    /// See [`WikiStore::replace_links`].
    pub fn replace_links(&self, slug: &str, links: &[(String, String)]) -> Result<()> {
        WikiStore::replace_links(&mut *self.backend.lock()?, slug, links)
    }

    /// See [`WikiStore::link_count`].
    pub fn link_count(&self, slug: &str) -> Result<usize> {
        WikiStore::link_count(&*self.backend.lock()?, slug)
    }

    /// See [`WikiStore::search`].
    pub fn search(&self, phrases: &[String], limit: usize) -> Result<Vec<WikiSearchHit>> {
        WikiStore::search(&*self.backend.lock()?, phrases, limit)
    }

    /// See [`WikiStore::meta`].
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        WikiStore::meta(&*self.backend.lock()?, key)
    }

    /// See [`WikiStore::set_meta`].
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        WikiStore::set_meta(&mut *self.backend.lock()?, key, value)
    }
}

/// The behaviour every [`WikiStore`] must show, run against each backend by
/// [`wiki_conformance`] (ADR-0035).
#[cfg(test)]
pub(crate) mod conformance {
    use super::WikiStore;
    use crate::wiki::WikiPage;

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

    pub(crate) fn an_unbacked_write_keeps_the_cached_mtime(wiki: &mut dyn WikiStore) {
        wiki.upsert(&page("a", 42.0, T1)).unwrap();
        wiki.upsert_unbacked("a", "A", "new", "", T2).unwrap();
        wiki.upsert_unbacked("b", "B", "body", "", T2).unwrap();

        let a = wiki.get("a").unwrap().unwrap();
        assert_eq!((a.content.as_str(), a.mtime), ("new", 42.0));
        assert_eq!(wiki.get("b").unwrap().unwrap().mtime, 0.0);
    }

    pub(crate) fn remove_takes_the_links_and_reports_a_missing_page(wiki: &mut dyn WikiStore) {
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

    pub(crate) fn meta_round_trips_and_overwrites(wiki: &mut dyn WikiStore) {
        assert_eq!(wiki.meta("k").unwrap(), None);
        wiki.set_meta("k", "1").unwrap();
        wiki.set_meta("k", "2").unwrap();
        assert_eq!(wiki.meta("k").unwrap().as_deref(), Some("2"));
    }

    pub(crate) fn listings_order_by_recency_then_title_ignoring_case(wiki: &mut dyn WikiStore) {
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
    pub(crate) fn search_finds_and_ranks_pages_and_a_rewrite_replaces_the_entry(
        wiki: &mut dyn WikiStore,
    ) {
        let corpus = [
            ("ownership", "Ownership", "Rust ownership rules: each value has one owner. Borrowing lends it without moving."),
            ("borrowing", "Borrowing", "Borrowing and borrowing again: shared borrows and mutable borrows."),
            ("lifetimes", "Lifetimes", "Lifetimes tie a reference to the owner it borrows from."),
            ("unrelated", "Gardening", "Tomatoes like sun, and so does borrowing light."),
        ];
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

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`. A backend's adapter invokes it in its own test module.
#[cfg(test)]
macro_rules! wiki_conformance {
    ($make:expr) => {
        use $crate::db::wiki::conformance;

        #[test]
        fn an_unbacked_write_keeps_the_cached_mtime() {
            conformance::an_unbacked_write_keeps_the_cached_mtime(&mut $make);
        }
        #[test]
        fn remove_takes_the_links_and_reports_a_missing_page() {
            conformance::remove_takes_the_links_and_reports_a_missing_page(&mut $make);
        }
        #[test]
        fn meta_round_trips_and_overwrites() {
            conformance::meta_round_trips_and_overwrites(&mut $make);
        }
        #[test]
        fn listings_order_by_recency_then_title_ignoring_case() {
            conformance::listings_order_by_recency_then_title_ignoring_case(&mut $make);
        }
        #[test]
        fn search_finds_and_ranks_pages_and_a_rewrite_replaces_the_entry() {
            conformance::search_finds_and_ranks_pages_and_a_rewrite_replaces_the_entry(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use wiki_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let wiki = WikiIndex::new(&store);
        let at = "2026-09-26T00:00:00+00:00";
        wiki.upsert_unbacked("a", "A", "ownership", "", at).unwrap();
        wiki.replace_links("a", &[("b".to_string(), "B".to_string())])
            .unwrap();
        wiki.set_meta("k", "v").unwrap();
        assert_eq!(wiki.get("a").unwrap().unwrap().title, "A");
        assert_eq!(wiki.link_count("a").unwrap(), 1);
        assert_eq!(wiki.meta("k").unwrap().as_deref(), Some("v"));
        assert_eq!(
            wiki.search(&crate::fts::query_phrases("ownership"), 5)
                .unwrap()
                .len(),
            1
        );
        assert!(wiki.remove("a").unwrap());
    }
}
