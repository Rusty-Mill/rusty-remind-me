//! The automatic wiki compile (card #747): grouping, marking only after every
//! page is written, and the guards that stop a batch without losing anything.

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::Store;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::wiki_compile::{run, AutoOptions, PageDraft, PageRequest, WikiWriter};
use remind_me_core::wiki_fs::{pending_compile_count, Wiki};
use remind_me_core::Database;
use std::cell::RefCell;

/// A wiki rooted in its own scratch directory.
fn wiki(name: &str) -> (Wiki, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("rrm_autowiki_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    (Wiki::new(&root), root)
}

fn remember(store: &Store<'_>, memories: &[(&str, &str)]) {
    for (n, (id, content)) in memories.iter().enumerate() {
        let at = format!("2026-11-05T12:00:{n:02}+00:00");
        Memories::new(store)
            .insert(&NewMemory::new(*id, content.to_string(), &at))
            .unwrap();
    }
}

/// Two topics: text mentioning "rust" and text mentioning "garden".
struct Topics;

impl Embedder for Topics {
    fn embed(&self, texts: &[String], _: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let t = t.to_lowercase();
                match (t.contains("rust"), t.contains("garden")) {
                    (true, _) => vec![1.0, 0.0],
                    (_, true) => vec![0.0, 1.0],
                    _ => vec![0.6, 0.6],
                }
            })
            .collect())
    }
    fn dim(&self) -> usize {
        2
    }
    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "test".into(),
            model: "topics".into(),
            dim: 2,
        }
    }
}

/// How the fake model misbehaves, if it does.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    Fails,
    Shrinks,
    ForgetsCitations,
    /// Rewrites a page without the citations it already had.
    DropsEarlierCitations,
}

/// Appends a cited line per source to the page, or titles a new page after
/// its first source's first word.
struct Scribe {
    fault: Fault,
    calls: RefCell<Vec<(Option<String>, Vec<String>)>>,
}

impl Scribe {
    fn new(fault: Fault) -> Self {
        Self {
            fault,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl WikiWriter for Scribe {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        let ids = request.sources.iter().map(|s| s.id.clone()).collect();
        self.calls
            .borrow_mut()
            .push((request.page.map(|p| p.title.clone()), ids));
        if self.fault == Fault::Fails {
            return Err("model offline".into());
        }
        let (title, mut content) = match request.page {
            Some(page) => (page.title.clone(), page.content.clone()),
            None => {
                let first = &request.sources[0].content;
                let word = first.split_whitespace().next().unwrap_or("Notes");
                (word.to_string(), String::new())
            }
        };
        if self.fault == Fault::Shrinks {
            content = "gone".into();
        }
        if self.fault == Fault::DropsEarlierCitations {
            content = content.replace(" (`mem_a`)", "");
        }
        for source in request.sources {
            match self.fault {
                Fault::ForgetsCitations => content.push_str(&format!("\n{}", source.content)),
                _ => content.push_str(&format!("\n{} (`{}`)", source.content, source.id)),
            }
        }
        Ok(PageDraft { title, content })
    }
}

#[test]
fn without_an_embedder_a_batch_becomes_one_page_and_is_marked() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("one-page");
    remember(
        &db.store(),
        &[
            ("mem_a", "Rust has ownership"),
            ("mem_b", "Garden needs water"),
        ],
    );

    let report = run(
        &w,
        &db,
        &Scribe::new(Fault::None),
        None,
        &AutoOptions::default(),
    )
    .unwrap();

    assert_eq!(report.stopped, None);
    assert_eq!((report.batches, report.sources_integrated), (1, 2));
    assert_eq!(report.pages_written, ["Rust"]);
    assert_eq!(report.pending, 0);
    let page = w.read_page(&db.store(), "Rust").unwrap().unwrap();
    assert!(page.content.contains("`mem_a`") && page.content.contains("`mem_b`"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_embedder_routes_sources_to_their_page_or_a_new_one() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("routing");
    w.write_page(&db.store(), "Rust", "Rust is a language.", None)
        .unwrap()
        .unwrap();
    remember(
        &db.store(),
        &[
            ("mem_a", "rust has ownership"),
            ("mem_b", "Garden needs water"),
        ],
    );
    let scribe = Scribe::new(Fault::None);

    let report = run(&w, &db, &scribe, Some(&Topics), &AutoOptions::default()).unwrap();

    assert_eq!(report.stopped, None);
    assert_eq!(report.pending, 0);
    let calls = scribe.calls.borrow();
    assert!(calls.contains(&(Some("Rust".into()), vec!["mem_a".into()])));
    assert!(calls.contains(&(None, vec!["mem_b".into()])));
    let rust = w.read_page(&db.store(), "Rust").unwrap().unwrap();
    assert!(rust.content.contains("Rust is a language.") && rust.content.contains("`mem_a`"));
    assert!(w.read_page(&db.store(), "Garden").unwrap().is_some());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_failed_batch_leaves_every_source_pending_and_frees_the_lock() {
    for fault in [Fault::Fails, Fault::Shrinks, Fault::ForgetsCitations] {
        let db = Database::open_in_memory().unwrap();
        let (w, root) = wiki("failed");
        let original = "Rust is a language with a long page. ".repeat(4);
        w.write_page(&db.store(), "Rust", &original, None)
            .unwrap()
            .unwrap();
        remember(&db.store(), &[("mem_a", "rust has ownership")]);

        let options = AutoOptions::default();
        let report = run(&w, &db, &Scribe::new(fault), Some(&Topics), &options).unwrap();

        assert!(report.stopped.is_some(), "{:?}", report.pages_written);
        assert_eq!((report.batches, report.pending), (0, 1));
        assert_eq!(pending_compile_count(&db.store()).unwrap(), 1);
        // The lock is released: a healthy writer picks the batch straight up.
        let retry = run(&w, &db, &Scribe::new(Fault::None), Some(&Topics), &options).unwrap();
        assert_eq!(
            (retry.batches, retry.pending),
            (1, 0),
            "{:?}",
            retry.stopped
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn a_shrinking_rewrite_is_refused_unless_allowed() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("shrink");
    let original = "Rust is a language with a long page. See [[Cargo]].";
    w.write_page(&db.store(), "Rust", original, None)
        .unwrap()
        .unwrap();
    remember(&db.store(), &[("mem_a", "rust has ownership")]);

    let refused = run(
        &w,
        &db,
        &Scribe::new(Fault::Shrinks),
        Some(&Topics),
        &AutoOptions::default(),
    )
    .unwrap();
    assert!(refused.stopped.unwrap().contains("keeps"));
    assert!(w
        .read_page(&db.store(), "Rust")
        .unwrap()
        .unwrap()
        .content
        .contains("[[Cargo]]"));

    let allowed = AutoOptions {
        allow_shrink: true,
        ..AutoOptions::default()
    };
    let report = run(
        &w,
        &db,
        &Scribe::new(Fault::Shrinks),
        Some(&Topics),
        &allowed,
    )
    .unwrap();
    assert_eq!((report.batches, report.pending), (1, 0));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_new_page_that_names_an_existing_one_is_guarded_as_a_rewrite() {
    // No embedder: the batch goes out as a new page, and the scribe titles
    // it after the first word, "Rust", which already exists.
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("collision");
    let original = "A long page about the language, with a lot of history in it.";
    w.write_page(&db.store(), "Rust", original, None)
        .unwrap()
        .unwrap();
    remember(&db.store(), &[("mem_a", "Rust has ownership")]);

    let report = run(
        &w,
        &db,
        &Scribe::new(Fault::None),
        None,
        &AutoOptions::default(),
    )
    .unwrap();

    assert!(report.stopped.unwrap().contains("keeps"));
    assert!(w
        .read_page(&db.store(), "Rust")
        .unwrap()
        .unwrap()
        .content
        .contains(original));
    assert_eq!(pending_compile_count(&db.store()).unwrap(), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

/// A writer that, mid-run, starts a second compile on the same wiki.
struct Reentrant<'a> {
    wiki: &'a Wiki,
    db: &'a Database,
    inner: Scribe,
    nested: RefCell<Option<remind_me_core::wiki_compile::AutoReport>>,
}

impl WikiWriter for Reentrant<'_> {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        let nested = run(
            self.wiki,
            self.db,
            &Scribe::new(Fault::None),
            None,
            &AutoOptions::default(),
        )
        .unwrap();
        *self.nested.borrow_mut() = Some(nested);
        self.inner.write_page(request)
    }
}

#[test]
fn a_second_run_on_the_same_wiki_stops_before_the_model_is_called() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("locked");
    remember(&db.store(), &[("mem_a", "Rust has ownership")]);
    let outer = Reentrant {
        wiki: &w,
        db: &db,
        inner: Scribe::new(Fault::None),
        nested: RefCell::new(None),
    };

    let report = run(&w, &db, &outer, None, &AutoOptions::default()).unwrap();

    let nested = outer.nested.borrow_mut().take().unwrap();
    assert!(nested.stopped.unwrap().contains("another compile"));
    assert_eq!(nested.batches, 0);
    // The first run carried on and finished; its claim was released after.
    assert_eq!((report.batches, report.pending), (1, 0));
    remember(&db.store(), &[("mem_b", "Rust has traits")]);
    // With topics the source joins the page rather than replacing it.
    let again = run(
        &w,
        &db,
        &Scribe::new(Fault::None),
        Some(&Topics),
        &AutoOptions::default(),
    )
    .unwrap();
    assert_eq!(again.stopped, None);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_citation_dropped_by_a_later_chunk_keeps_the_batch_pending() {
    // Two 7,000-character sources exceed one call's budget, so one new page
    // is written in two calls. The second keeps the page's length and links
    // but drops `mem_a`'s citation. The guard refuses that draft, the retry
    // drops it again, and the batch stays pending with the page as the first
    // call left it.
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("dropped-citation");
    let long_a = format!("Rust {}", "a".repeat(7_000));
    let long_b = format!("Rust {}", "b".repeat(7_000));
    remember(&db.store(), &[("mem_a", &long_a), ("mem_b", &long_b)]);
    let scribe = Scribe::new(Fault::DropsEarlierCitations);

    let report = run(&w, &db, &scribe, None, &AutoOptions::default()).unwrap();

    assert_eq!(scribe.calls.borrow().len(), 3);
    assert!(report.stopped.unwrap().contains("drops `mem_a`"));
    assert_eq!((report.batches, report.pending), (0, 2));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn max_batches_bounds_a_run() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("bounded");
    remember(&db.store(), &[("mem_a", "Rust has ownership")]);
    let none = AutoOptions {
        max_batches: 0,
        ..AutoOptions::default()
    };

    let report = run(&w, &db, &Scribe::new(Fault::None), None, &none).unwrap();

    assert_eq!((report.batches, report.pending), (0, 1));
    std::fs::remove_dir_all(&root).unwrap();
}

/// A writer that, while it "generates", has another thread write a memory
/// through its own store handle, as an unrelated tool call would.
struct Concurrent<'a> {
    db: &'a Database,
    inner: Scribe,
    other_write_finished: RefCell<bool>,
}

impl WikiWriter for Concurrent<'_> {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                remember(&self.db.store(), &[("mem_other", "written mid-compile")]);
                let _ = done.send(());
            });
            *self.other_write_finished.borrow_mut() = finished
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok();
        });
        self.inner.write_page(request)
    }
}

#[test]
fn other_callers_reach_the_store_while_the_model_writes() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("concurrent");
    remember(&db.store(), &[("mem_a", "Rust has ownership")]);
    let writer = Concurrent {
        db: &db,
        inner: Scribe::new(Fault::None),
        other_write_finished: RefCell::new(false),
    };

    let one = AutoOptions {
        max_batches: 1,
        ..AutoOptions::default()
    };

    let report = run(&w, &db, &writer, None, &one).unwrap();

    assert!(
        *writer.other_write_finished.borrow(),
        "the store stayed locked"
    );
    // The batch was fixed before the other write, which stays pending.
    assert_eq!((report.batches, report.pending), (1, 1));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_page_edited_while_the_model_writes_stops_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("edited-meanwhile");
    let original = "Rust is a language with a long page. ".repeat(4);
    w.write_page(&db.store(), "Rust", &original, None)
        .unwrap()
        .unwrap();
    remember(&db.store(), &[("mem_a", "rust has ownership")]);
    let editor = Editor {
        wiki: &w,
        db: &db,
        inner: Scribe::new(Fault::None),
    };

    let report = run(&w, &db, &editor, Some(&Topics), &AutoOptions::default()).unwrap();

    assert!(report.stopped.unwrap().contains("changed while"));
    assert_eq!(report.pending, 1);
    let page = w.read_page(&db.store(), "Rust").unwrap().unwrap();
    assert!(page.content.contains("edited by hand"));
    std::fs::remove_dir_all(&root).unwrap();
}

/// A writer during whose call someone edits the page by hand.
struct Editor<'a> {
    wiki: &'a Wiki,
    db: &'a Database,
    inner: Scribe,
}

impl WikiWriter for Editor<'_> {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        let draft = self.inner.write_page(request)?;
        let page = request.page.expect("an update of the existing page");
        let edited = format!("{}\nedited by hand", page.content);
        self.wiki
            .write_page(&self.db.store(), &page.title, &edited, None)
            .unwrap()
            .unwrap();
        Ok(draft)
    }
}

/// A writer during whose call someone edits a source memory.
struct SourceEditor<'a> {
    db: &'a Database,
    inner: Scribe,
}

impl WikiWriter for SourceEditor<'_> {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        remind_me_core::db::queries::update_memory(
            &self.db.store(),
            &remind_me_core::MemoryUpdateInput {
                memory_id: "mem_a".into(),
                content: Some("Rust has ownership and borrowing".into()),
                category: None,
                tags: None,
                metadata: None,
                sensitive: None,
                clear_superseded: false,
                project: None,
            },
        )
        .unwrap();
        self.inner.write_page(request)
    }
}

#[test]
fn a_source_edited_while_the_model_writes_stays_pending() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("source-edited");
    remember(&db.store(), &[("mem_a", "Rust has ownership")]);
    let writer = SourceEditor {
        db: &db,
        inner: Scribe::new(Fault::None),
    };
    let one = AutoOptions {
        max_batches: 1,
        ..AutoOptions::default()
    };

    let report = run(&w, &db, &writer, None, &one).unwrap();

    assert!(report.stopped.unwrap().contains("`mem_a` was edited"));
    assert_eq!((report.batches, report.pending), (0, 1));
    std::fs::remove_dir_all(&root).unwrap();
}

/// Drops the page's existing citations on its first draft, and keeps them
/// once told why that draft was refused.
struct Forgetful {
    feedback: RefCell<Vec<Option<String>>>,
}

impl WikiWriter for Forgetful {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        self.feedback
            .borrow_mut()
            .push(request.feedback.map(str::to_string));
        let page = request.page.expect("an update");
        let mut content = match request.feedback {
            None => page.content.replace("`9001`", "its port"),
            Some(_) => page.content.clone(),
        };
        for source in request.sources {
            content.push_str(&format!("\n{} (`{}`)", source.content, source.id));
        }
        Ok(PageDraft {
            title: page.title.clone(),
            content,
        })
    }
}

#[test]
fn a_refused_draft_is_retried_once_with_the_reason() {
    let db = Database::open_in_memory().unwrap();
    let (w, root) = wiki("retry-with-reason");
    let original = "# Rust\n\nThe rust bridge listens on `9001`, a fact worth keeping.";
    w.write_page(&db.store(), "Rust", original, None)
        .unwrap()
        .unwrap();
    remember(&db.store(), &[("mem_a", "rust has ownership")]);
    let writer = Forgetful {
        feedback: RefCell::new(Vec::new()),
    };

    let report = run(&w, &db, &writer, Some(&Topics), &AutoOptions::default()).unwrap();

    assert_eq!(
        (report.batches, report.pending),
        (1, 0),
        "{:?}",
        report.stopped
    );
    let feedback = writer.feedback.borrow();
    assert_eq!(feedback.len(), 2);
    assert_eq!(feedback[0], None);
    assert!(feedback[1].as_deref().unwrap().contains("drops `9001`"));
    let saved = w.read_page(&db.store(), "Rust").unwrap().unwrap().content;
    assert!(saved.contains("`9001`") && saved.contains("`mem_a`"));
    std::fs::remove_dir_all(&root).unwrap();
}
