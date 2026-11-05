//! Automatic wiki compile: pending memories become pages without a person
//! carrying the brief to a model (card #747).
//!
//! A run takes the same batch a brief would ([`Wiki::pending_batch`]),
//! groups its sources by the page they belong to, has a [`WikiWriter`] write
//! each page, and marks the batch integrated through its `through` only after
//! every page is saved. A failed batch leaves the watermark where it was, so
//! the next run hands the model the same sources again.
//!
//! Everything here is policy: grouping, the guards on a rewrite and the run
//! loop. The model is behind [`WikiWriter`] (`wiki_llm` has the
//! OpenAI-compatible adapter) and the vectors behind [`Embedder`], so tests
//! drive the loop with fakes.

use crate::db::memories::CreatedMemory;
use crate::db::{Database, Store};
use crate::embedder::{EmbedRole, Embedder};
use crate::wiki::WikiPage;
use crate::wiki_fs::{Wiki, WikiCompile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// Source characters one writer call carries at most; a larger group is
/// written in consecutive calls on the same page.
pub const GROUP_CHARS: usize = 12_000;
/// Cosine a source needs to join an existing page by default.
pub const DEFAULT_MATCH: f32 = 0.55;
/// A second page also takes a source when it is this close to the best.
const SECOND_PAGE_MARGIN: f32 = 0.05;
/// A rewrite may not cut an existing page below this share of its length.
const MIN_KEPT: f64 = 0.6;
/// Wiki roots with a compile running. Only one process opens a store (the
/// engine holds a lock for it), so an in-process set excludes every other
/// run on the same wiki, atomically and for as long as the run takes.
static RUNNING: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
/// Memories a batch reads before the character budget trims it.
const BATCH_LIMIT: usize = 100;

/// One source as the writer sees it.
pub type Source = CreatedMemory;

/// What one writer call is asked to produce.
#[derive(Debug, Clone)]
pub struct PageRequest<'a> {
    /// The maintainer schema (`schema.md`).
    pub schema: &'a str,
    /// The page to update; `None` asks for a new page the writer names.
    pub page: Option<&'a WikiPage>,
    pub sources: &'a [Source],
}

/// A page as the writer returns it: the whole new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDraft {
    pub title: String,
    pub content: String,
}

/// Writes one page from its sources. The port the model sits behind.
pub trait WikiWriter {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String>;
}

/// How a run behaves.
#[derive(Debug, Clone)]
pub struct AutoOptions {
    /// Batches to compile before stopping.
    pub max_batches: usize,
    /// Accept a rewrite that shrinks a page or drops one of its links.
    pub allow_shrink: bool,
    /// Cosine a source needs to join an existing page.
    pub match_threshold: f32,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            max_batches: 5,
            allow_shrink: false,
            match_threshold: DEFAULT_MATCH,
        }
    }
}

/// What a run did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoReport {
    /// Batches marked integrated.
    pub batches: usize,
    pub sources_integrated: usize,
    /// Titles written, in order; a page written twice appears twice.
    pub pages_written: Vec<String>,
    /// Pending sources left after the run.
    pub pending: usize,
    /// Why the run stopped early: a batch failed and was left pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

/// Sources bound for one page: an existing one, or a new one (`page: None`).
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub page: Option<usize>,
    pub sources: Vec<usize>,
}

/// Compile pending memories into pages until none are left, a batch fails,
/// or `max_batches` is reached. Errors are store errors; a writer or guard
/// failure stops the run and is reported in [`AutoReport::stopped`].
///
/// Takes the database, not a store handle: a handle holds the database lock
/// while it lives, and a model call can take minutes. Each store step takes
/// its own short handle, and no handle is held while the writer runs, so
/// other callers keep reading and writing in between.
pub fn run(
    wiki: &Wiki,
    db: &Database,
    writer: &dyn WikiWriter,
    embedder: Option<&dyn Embedder>,
    options: &AutoOptions,
) -> crate::db::Result<AutoReport> {
    let mut report = AutoReport::default();
    let Some(_running) = RunGuard::take(wiki.root()) else {
        report.stopped = Some("another compile is running on this wiki".to_string());
        report.pending = crate::wiki_fs::pending_compile_count(&db.store())?;
        return Ok(report);
    };
    run_batches(wiki, db, writer, embedder, options, &mut report)?;
    report.pending = crate::wiki_fs::pending_compile_count(&db.store())?;
    Ok(report)
}

fn run_batches(
    wiki: &Wiki,
    db: &Database,
    writer: &dyn WikiWriter,
    embedder: Option<&dyn Embedder>,
    options: &AutoOptions,
    report: &mut AutoReport,
) -> crate::db::Result<()> {
    for _ in 0..options.max_batches {
        let Some(batch) = wiki.pending_batch(&db.store(), BATCH_LIMIT)? else {
            return Ok(());
        };
        match write_batch(wiki, db, writer, embedder, options, &batch.sources)? {
            Err(why) => {
                report.stopped = Some(why);
                return Ok(());
            }
            Ok(titles) => report.pages_written.extend(titles),
        }
        // Checked and marked under one handle, so nothing changes between.
        let store = db.store();
        if let Some(id) = changed_source(&store, &batch.sources)? {
            report.stopped = Some(format!(
                "source `{id}` was edited while its pages were written"
            ));
            return Ok(());
        }
        if let WikiCompile::Integrated { sources_marked, .. } =
            wiki.compile(&store, BATCH_LIMIT, true, Some(&batch.through))?
        {
            report.sources_integrated += sources_marked;
        }
        report.batches += 1;
    }
    Ok(())
}

/// Write every page one batch needs. `Ok(Err(why))` is a failed batch.
fn write_batch(
    wiki: &Wiki,
    db: &Database,
    writer: &dyn WikiWriter,
    embedder: Option<&dyn Embedder>,
    options: &AutoOptions,
    sources: &[Source],
) -> crate::db::Result<Result<Vec<String>, String>> {
    let schema = wiki.read_schema()?;
    let pages = wiki.list_pages(&db.store())?;
    let groups = group(&pages, sources, embedder, options.match_threshold);
    let mut titles = Vec::new();
    for group in groups {
        let mut page = group.page.map(|i| pages[i].clone());
        for chunk in chunks(&group.sources, sources) {
            let request = PageRequest {
                schema: &schema,
                page: page.as_ref(),
                sources: &chunk,
            };
            // No store handle is held here: this is the slow step.
            let draft = match writer.write_page(&request) {
                Ok(draft) => draft,
                Err(e) => return Ok(Err(format!("the writer failed: {e}"))),
            };
            let store = db.store();
            if let Err(why) = save_draft(wiki, &store, page.as_ref(), &draft, chunk.len(), options)?
            {
                return Ok(Err(why));
            }
            titles.push(draft.title.clone());
            page = wiki.read_page(&store, &draft.title)?;
        }
    }
    // Checked against the pages as saved, not the drafts: a later rewrite of
    // a page may have dropped a citation an earlier chunk wrote.
    let store = db.store();
    let mut written = String::new();
    for title in titles.iter().collect::<BTreeSet<_>>() {
        if let Some(page) = wiki.read_page(&store, title)? {
            written.push_str(&page.content);
        }
    }
    Ok(match uncited(sources, &written) {
        Some(id) => Err(format!("no page written cites source `{id}`")),
        None => Ok(titles),
    })
}

/// The first source whose text changed since the batch was read. A source
/// deleted since is not a change: marking accepts it, as for a brief.
fn changed_source<'a>(
    store: &Store<'_>,
    sources: &'a [Source],
) -> crate::db::Result<Option<&'a str>> {
    for source in sources {
        let now = crate::db::queries::get_memory_by_id(store, &source.id)?;
        if now.is_some_and(|memory| memory.content != source.content) {
            return Ok(Some(&source.id));
        }
    }
    Ok(None)
}

/// Check `draft` against the page as it stands now and save it, all under
/// one store handle. `seen` is the page the writer was shown: if it changed
/// while the writer ran, saving would discard that change, so the batch
/// stops instead. A new-page draft that names an existing page is a rewrite
/// of it, and gets the same guards.
fn save_draft(
    wiki: &Wiki,
    store: &Store<'_>,
    seen: Option<&WikiPage>,
    draft: &PageDraft,
    source_count: usize,
    options: &AutoOptions,
) -> crate::db::Result<Result<(), String>> {
    let current = match seen {
        Some(seen) => wiki.read_page(store, &seen.title)?,
        None => wiki.read_page(store, &draft.title)?,
    };
    if let Some(seen) = seen {
        if current.as_ref().map(|page| &page.content) != Some(&seen.content) {
            return Ok(Err(format!(
                "[[{}]] changed while it was being written",
                seen.title
            )));
        }
    }
    if let Err(why) = check_draft(current.as_ref(), draft, options.allow_shrink) {
        return Ok(Err(why));
    }
    let note = format!("auto-compiled from {source_count} source(s)");
    if wiki
        .write_page(store, &draft.title, &draft.content, Some(&note))?
        .is_err()
    {
        return Ok(Err(format!("{:?} is a reserved page name", draft.title)));
    }
    Ok(Ok(()))
}

/// Group sources by page. Each source joins its closest page at
/// `threshold` or above, and a second page within a small margin of it;
/// the rest gather, in order, around the first left unplaced. Without an
/// embedder, or if embedding fails, the batch is one new-page group and the
/// writer decides.
pub fn group(
    pages: &[WikiPage],
    sources: &[Source],
    embedder: Option<&dyn Embedder>,
    threshold: f32,
) -> Vec<Group> {
    let everything = || {
        vec![Group {
            page: None,
            sources: (0..sources.len()).collect(),
        }]
    };
    let Some(embedder) = embedder else {
        return everything();
    };
    let Some((page_vecs, source_vecs)) = embed_all(embedder, pages, sources) else {
        return everything();
    };

    let mut by_page: Vec<Vec<usize>> = vec![Vec::new(); pages.len()];
    let mut unplaced = Vec::new();
    for (s, vector) in source_vecs.iter().enumerate() {
        let mut scored: Vec<(usize, f32)> = page_vecs
            .iter()
            .enumerate()
            .map(|(p, page)| (p, dot(vector, page)))
            .filter(|(_, score)| *score >= threshold)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let Some(&(best, top)) = scored.first() else {
            unplaced.push(s);
            continue;
        };
        by_page[best].push(s);
        if let Some(&(second, score)) = scored.get(1) {
            if top - score <= SECOND_PAGE_MARGIN {
                by_page[second].push(s);
            }
        }
    }

    let mut groups: Vec<Group> = by_page
        .into_iter()
        .enumerate()
        .filter(|(_, members)| !members.is_empty())
        .map(|(p, members)| Group {
            page: Some(p),
            sources: members,
        })
        .collect();
    groups.extend(cluster(&unplaced, &source_vecs, threshold));
    groups
}

/// Gather `unplaced` sources, in order, around the first one left over.
fn cluster(unplaced: &[usize], vectors: &[Vec<f32>], threshold: f32) -> Vec<Group> {
    let mut left: Vec<usize> = unplaced.to_vec();
    let mut groups = Vec::new();
    while let Some(&seed) = left.first() {
        let (members, rest): (Vec<usize>, Vec<usize>) = left
            .iter()
            .partition(|&&s| s == seed || dot(&vectors[s], &vectors[seed]) >= threshold);
        groups.push(Group {
            page: None,
            sources: members,
        });
        left = rest;
    }
    groups
}

type Vectors = Vec<Vec<f32>>;

/// Page keys (`title` and summary) and sources, embedded in one call.
fn embed_all(
    embedder: &dyn Embedder,
    pages: &[WikiPage],
    sources: &[Source],
) -> Option<(Vectors, Vectors)> {
    let texts: Vec<String> = pages
        .iter()
        .map(|p| format!("{}\n{}", p.title, p.summary))
        .chain(sources.iter().map(|s| s.content.clone()))
        .collect();
    let mut vectors = embedder.embed(&texts, EmbedRole::Passage).ok()?;
    if vectors.len() != texts.len() {
        return None;
    }
    let source_vecs = vectors.split_off(pages.len());
    Some((vectors, source_vecs))
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A group's sources in writer calls of at most [`GROUP_CHARS`]; a source
/// longer than that goes alone, whole.
fn chunks(members: &[usize], sources: &[Source]) -> Vec<Vec<Source>> {
    let mut out: Vec<Vec<Source>> = Vec::new();
    let mut used = 0;
    for &i in members {
        let source = &sources[i];
        let size = source.content.chars().count();
        let fits = out.last().is_some_and(|c| !c.is_empty()) && used + size <= GROUP_CHARS;
        if !fits {
            out.push(Vec::new());
            used = 0;
        }
        used += size;
        if let Some(last) = out.last_mut() {
            last.push(source.clone());
        }
    }
    out
}

/// Refuse a draft that would lose what a page held: an empty page, a renamed
/// page, or, unless allowed, a page cut below [`MIN_KEPT`] of its length or
/// missing one of its `[[links]]`.
pub fn check_draft(
    page: Option<&WikiPage>,
    draft: &PageDraft,
    allow_shrink: bool,
) -> Result<(), String> {
    if draft.title.trim().is_empty() || draft.content.trim().is_empty() {
        return Err("the writer returned an empty page".to_string());
    }
    let Some(page) = page else {
        return Ok(());
    };
    if !draft.title.trim().eq_ignore_ascii_case(page.title.trim()) {
        return Err(format!(
            "the writer renamed [[{}]] to [[{}]]",
            page.title, draft.title
        ));
    }
    if allow_shrink {
        return Ok(());
    }
    let before = page.content.chars().count() as f64;
    let after = draft.content.chars().count() as f64;
    if after < before * MIN_KEPT {
        return Err(format!(
            "the rewrite of [[{}]] keeps {:.0}% of its text",
            page.title,
            100.0 * after / before
        ));
    }
    if let Some(link) = links(&page.content)
        .into_iter()
        .find(|link| !draft.content.contains(link.as_str()))
    {
        return Err(format!(
            "the rewrite of [[{}]] drops the link {link}",
            page.title
        ));
    }
    Ok(())
}

/// Every `[[...]]` in `text`, as written.
fn links(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start..].find("]]") else {
            break;
        };
        found.push(rest[start..start + len + 2].to_string());
        rest = &rest[start + len + 2..];
    }
    found
}

/// The first source whose id appears in no written page.
fn uncited<'a>(sources: &'a [Source], written: &str) -> Option<&'a str> {
    sources
        .iter()
        .map(|s| s.id.as_str())
        .find(|id| !written.contains(id))
}

/// A running compile's claim on its wiki root, released when dropped.
struct RunGuard(PathBuf);

impl RunGuard {
    /// The claim on `root`, or `None` while another run holds it.
    fn take(root: &std::path::Path) -> Option<Self> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        running
            .insert(root.to_path_buf())
            .then(|| Self(root.to_path_buf()))
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str, content: &str) -> WikiPage {
        WikiPage {
            slug: title.to_lowercase(),
            title: title.into(),
            content: content.into(),
            summary: String::new(),
            mtime: 0.0,
            updated_at: String::new(),
        }
    }

    fn draft(title: &str, content: &str) -> PageDraft {
        PageDraft {
            title: title.into(),
            content: content.into(),
        }
    }

    #[test]
    fn a_new_page_needs_only_text() {
        assert!(check_draft(None, &draft("Rust", "text"), false).is_ok());
        assert!(check_draft(None, &draft("Rust", "  "), false).is_err());
    }

    #[test]
    fn a_rewrite_may_not_rename_shrink_or_drop_links() {
        let old = page("Rust", "Rust is a language. See [[Cargo]] and [[Clippy]].");
        assert!(check_draft(Some(&old), &draft("Go", &old.content), false)
            .unwrap_err()
            .contains("renamed"));
        assert!(check_draft(Some(&old), &draft("Rust", "Rust."), false)
            .unwrap_err()
            .contains("keeps"));
        let dropped = "Rust is a language. See [[Cargo]] and the linter, at length.";
        assert!(check_draft(Some(&old), &draft("rust", dropped), false)
            .unwrap_err()
            .contains("[[Clippy]]"));
        assert!(check_draft(Some(&old), &draft("Rust", "Rust."), true).is_ok());
    }

    #[test]
    fn links_are_found_in_order() {
        assert_eq!(links("a [[B]] c [[D e]] [[open"), ["[[B]]", "[[D e]]"]);
    }

    fn source(id: &str, len: usize) -> Source {
        Source {
            id: id.into(),
            category: "fact".into(),
            content: "x".repeat(len),
            created_at: String::new(),
        }
    }

    #[test]
    fn chunks_respect_the_budget_and_keep_a_long_source_whole() {
        let sources = [
            source("a", 7_000),
            source("b", 7_000),
            source("c", 20_000),
            source("d", 10),
        ];
        let ids: Vec<Vec<String>> = chunks(&[0, 1, 2, 3], &sources)
            .into_iter()
            .map(|c| c.into_iter().map(|s| s.id).collect())
            .collect();
        assert_eq!(ids, [vec!["a"], vec!["b"], vec!["c"], vec!["d"]]);
        assert_eq!(chunks(&[0, 3], &sources).len(), 1);
    }

    #[test]
    fn a_wiki_root_is_claimed_by_one_run_at_a_time() {
        let root = std::path::Path::new("/tmp/rrm-claim-test");
        let first = RunGuard::take(root).unwrap();
        assert!(RunGuard::take(root).is_none());
        assert!(RunGuard::take(std::path::Path::new("/tmp/rrm-other")).is_some());
        drop(first);
        assert!(RunGuard::take(root).is_some());
    }

    #[test]
    fn uncited_names_the_first_source_missing() {
        let sources = [source("mem_a", 1), source("mem_b", 1)];
        assert_eq!(uncited(&sources, "cites `mem_a`"), Some("mem_b"));
        assert_eq!(uncited(&sources, "mem_a mem_b"), None);
    }
}
