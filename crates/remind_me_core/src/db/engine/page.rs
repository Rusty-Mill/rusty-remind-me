//! Pages: several core writes that land together or not at all (ADR-0023,
//! core PR 4b). An importer writes a page of items — memories, graph
//! links, tracking rows — inside one SQLite transaction; on the core, the
//! same page is one journal batch.
//!
//! A page's writes must be readable as it goes (an importer checks for an
//! entity it upserted two items earlier), so they reach the stores at once
//! rather than waiting for the batch. What makes that safe is an undo log
//! beside the redo journal:
//!
//! 1. Before each write reaches the stores, the records it replaces go to
//!    the undo log, synced.
//! 2. Finishing the page commits every write as one redo batch, then
//!    empties the undo log, then checkpoints the redo journal.
//! 3. Abandoning the page puts the replaced records back, newest first,
//!    and empties the undo log.
//!
//! Opening the tables undoes whatever the undo log still holds, then
//! replays the redo journal. So a crash before the redo commit leaves no
//! trace of the page, and a crash after it leaves all of it.
//!
//! While a page is open, only the thread that opened it may write to the
//! core; another thread's write is refused rather than folded into a page
//! it did not ask for.

use super::core::{apply, decode, encode, Change, CoreTables};
use super::{engine_error, EngineTables};
use crate::db::{Result, StoreError};
use parking_lot::Mutex;
use rusty_multimodal_db_engine::generic::query::GetById;
use rusty_multimodal_db_engine::journal::Batch;
use std::thread::ThreadId;

/// The undo log inside the data directory.
pub(super) const UNDO_FILE: &str = "node.undo";

/// An open page: its owner, its writes, and what they replaced.
pub(super) struct OpenPage {
    owner: ThreadId,
    changes: Vec<Change>,
    /// The record each write replaced, oldest first.
    before: Vec<Change>,
    /// Set when a write failed part-way: the page can then only be
    /// abandoned.
    broken: bool,
}

/// The change that puts back what `change` is about to replace.
fn before_image(core: &CoreTables, change: &Change) -> Change {
    match change {
        Change::Memory(id, _) => Change::Memory(*id, core.memories.get(*id).map(Box::new)),
        Change::Outbox(id, _) => Change::Outbox(*id, core.outbox.get(*id)),
        Change::Send(id, _) => Change::Send(*id, core.sends.get(*id)),
        Change::Flag(id, _) => Change::Flag(*id, core.flags.get(*id)),
        Change::Delivery(id, _) => Change::Delivery(*id, core.deliveries.get(*id)),
        Change::Feedback(id, _) => Change::Feedback(*id, core.feedback.get(*id)),
        Change::Entity(id, _) => Change::Entity(*id, core.entities.get(*id).map(Box::new)),
        Change::Link(id, _) => Change::Link(*id, core.links.get(*id)),
        Change::Relation(id, _) => Change::Relation(*id, core.relations.get(*id).map(Box::new)),
        Change::Association(id, _) => Change::Association(*id, core.associations.get(*id)),
        Change::Promotion(id, _) => Change::Promotion(*id, core.promotions.get(*id)),
        Change::Chunk(id, _) => Change::Chunk(*id, core.chunks.get(*id).map(Box::new)),
        Change::EmbeddingMeta(id, _) => Change::EmbeddingMeta(*id, core.embedding_meta.get(*id)),
        Change::ChatImport(id, _) => {
            Change::ChatImport(*id, core.chat_imports.get(*id).map(Box::new))
        }
        Change::DbsImport(id, _) => Change::DbsImport(*id, core.dbs_imports.get(*id).map(Box::new)),
        Change::MempalaceImport(id, _) => {
            Change::MempalaceImport(*id, core.mempalace_imports.get(*id))
        }
    }
}

impl EngineTables {
    /// Open a page for the calling thread.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the tables refuse writes or already have a
    /// page open.
    pub(crate) fn begin_page(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if self.page.is_some() {
            return Err(StoreError::Engine(
                "a page is already open on these tables".to_string(),
            ));
        }
        self.page = Some(OpenPage {
            owner: std::thread::current().id(),
            changes: Vec::new(),
            before: Vec::new(),
            broken: false,
        });
        Ok(())
    }

    /// Whether a commit goes into the open page: `Ok(true)` for the page's
    /// own thread, `Ok(false)` with no page open.
    pub(super) fn in_page(&self) -> Result<bool> {
        match &self.page {
            None => Ok(false),
            Some(page) if page.owner == std::thread::current().id() => Ok(true),
            Some(_) => Err(StoreError::Engine(
                "another thread holds a page open on the memories core".to_string(),
            )),
        }
    }

    /// Write `changes` into the open page: their before-images to the undo
    /// log, synced, then the changes to the stores.
    pub(super) fn commit_in_page(&mut self, changes: Vec<Change>) -> Result<()> {
        let core = &mut self.core;
        let Some(page) = self.page.as_mut() else {
            return Err(StoreError::Engine("no page is open".to_string()));
        };
        let before: Vec<Change> = changes.iter().map(|c| before_image(core, c)).collect();
        self.undo.commit(&encode(&before)?).map_err(engine_error)?;
        page.before.extend(before);
        for change in changes {
            page.changes.push(change.clone());
            if let Err(e) = apply(core, change) {
                // The undo log covers this write; abandoning the page, or
                // reopening, puts the stores back.
                page.broken = true;
                return Err(e);
            }
        }
        Ok(())
    }

    /// Finish the open page: its writes become one redo batch.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if no page is open or a journal fails. A
    /// failure leaves the tables refusing writes until they are reopened,
    /// which settles the page one way or the other.
    pub(crate) fn finish_page(&mut self) -> Result<()> {
        let Some(page) = self.page.take() else {
            return Err(StoreError::Engine("no page is open".to_string()));
        };
        if page.broken {
            self.page = Some(page);
            self.abandon_page()?;
            return Err(StoreError::Engine(
                "a write in the page failed; the page was rolled back".to_string(),
            ));
        }
        let settled = self.settle(&page);
        if settled.is_err() {
            self.failed = true;
        }
        settled
    }

    fn settle(&mut self, page: &OpenPage) -> Result<()> {
        if !page.changes.is_empty() {
            self.journal
                .commit(&encode(&page.changes)?)
                .map_err(engine_error)?;
        }
        self.undo.checkpoint().map_err(engine_error)?;
        self.journal.checkpoint().map_err(engine_error)
    }

    /// Abandon the open page: put back every record it replaced.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if no page is open, or putting a record back
    /// fails; the tables then refuse writes until reopened, which finishes
    /// the rollback from the undo log.
    pub(crate) fn abandon_page(&mut self) -> Result<()> {
        let Some(page) = self.page.take() else {
            return Err(StoreError::Engine("no page is open".to_string()));
        };
        let rolled_back = self.put_back(page.before);
        if rolled_back.is_err() {
            self.failed = true;
        }
        rolled_back
    }

    fn put_back(&mut self, before: Vec<Change>) -> Result<()> {
        let core = &mut self.core;
        for change in before.into_iter().rev() {
            apply(core, change)?;
        }
        self.undo.checkpoint().map_err(engine_error)
    }

    /// Undo what an unfinished page left in the stores, from the undo log
    /// a crash left behind. Runs at open, before the redo replay.
    pub(super) fn roll_back(&mut self, undo: Vec<Batch>) -> Result<()> {
        let undo: Vec<Batch> = undo.into_iter().filter(|b| !b.is_empty()).collect();
        if undo.is_empty() {
            return Ok(());
        }
        for batch in undo.iter().rev() {
            for change in decode(batch)?.into_iter().rev() {
                apply(&mut self.core, change)?;
            }
        }
        self.undo.checkpoint().map_err(engine_error)
    }
}

/// An open page on shared tables: finished by [`Page::finish`], abandoned
/// when dropped unfinished (an error or a panic in the work it wraps).
pub(crate) struct Page<'t> {
    tables: &'t Mutex<EngineTables>,
    open: bool,
}

impl<'t> Page<'t> {
    pub(crate) fn begin(tables: &'t Mutex<EngineTables>) -> Result<Self> {
        tables.lock().begin_page()?;
        Ok(Self { tables, open: true })
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.open = false;
        self.tables.lock().finish_page()
    }
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        if let Err(e) = self.tables.lock().abandon_page() {
            // Nothing to return it to: the tables now refuse writes, and
            // reopening them finishes the rollback from the undo log.
            eprintln!("memories core: abandoning a page failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine::{memories, reopen, vectors};
    use crate::db::memories::NewMemory;
    use rusty_multimodal_db_engine::fulltext::Query;
    use std::path::PathBuf;

    const NOW: &str = "2026-09-27T00:00:00+00:00";

    fn fresh_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remind_me_page_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A chunk stored before the page, then a page that replaces it and
    /// adds a memory.
    fn write_a_page(tables: &mut EngineTables) {
        vectors::put(tables, "a", 0, &[1]).unwrap();
        tables.begin_page().unwrap();
        vectors::put(tables, "a", 0, &[2]).unwrap();
        memories::insert(tables, &NewMemory::new("m", "quokka", NOW)).unwrap();
    }

    fn landed(tables: &EngineTables) -> (Option<Vec<u8>>, bool, usize) {
        let core = &tables.core;
        (
            vectors::any_embedding(tables).unwrap(),
            memories::row(core, "m").is_some(),
            core.search.search(&Query::any_of(["quokka"])).len(),
        )
    }

    #[test]
    fn a_page_is_readable_as_it_goes_and_lands_when_finished() {
        let dir = fresh_dir("finished");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_a_page(&mut tables);
            assert_eq!(landed(&tables), (Some(vec![2]), true, 1));
            tables.finish_page().unwrap();
        }
        let tables = reopen(&dir);
        assert_eq!(landed(&tables), (Some(vec![2]), true, 1));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_abandoned_page_puts_back_what_it_replaced() {
        let dir = fresh_dir("abandoned");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_a_page(&mut tables);
            tables.abandon_page().unwrap();
            assert_eq!(landed(&tables), (Some(vec![1]), false, 0));
            // The tables take writes again.
            vectors::put(&mut tables, "b", 0, &[3]).unwrap();
        }
        let tables = reopen(&dir);
        assert_eq!(landed(&tables), (Some(vec![1]), false, 0));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_inside_a_page_leaves_no_trace_of_it() {
        let dir = fresh_dir("crash_inside");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_a_page(&mut tables);
            // Dropped with the page open: the process died here.
        }
        let tables = reopen(&dir);
        assert_eq!(landed(&tables), (Some(vec![1]), false, 0));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_after_the_redo_commit_keeps_the_whole_page() {
        let dir = fresh_dir("crash_after_commit");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_a_page(&mut tables);
            // The redo batch is durable; the undo log was never emptied.
            let changes = tables.page.as_ref().unwrap().changes.clone();
            tables.journal.commit(&encode(&changes).unwrap()).unwrap();
        }
        let tables = reopen(&dir);
        assert_eq!(landed(&tables), (Some(vec![2]), true, 1));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn another_thread_cannot_write_into_a_page() {
        let tables = Mutex::new(EngineTables::open_temporary().unwrap());
        let page = Page::begin(&tables).unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let refused = vectors::put(&mut tables.lock(), "x", 0, &[1]);
                    assert!(
                        matches!(&refused, Err(StoreError::Engine(why)) if why.contains("another thread")),
                        "{refused:?}"
                    );
                })
                .join()
                .unwrap();
        });
        vectors::put(&mut tables.lock(), "y", 0, &[1]).unwrap();
        page.finish().unwrap();
        assert_eq!(vectors::count(&tables.lock()).unwrap(), 1);
    }

    #[test]
    fn a_page_dropped_unfinished_is_abandoned() {
        let tables = Mutex::new(EngineTables::open_temporary().unwrap());
        {
            let _page = Page::begin(&tables).unwrap();
            vectors::put(&mut tables.lock(), "y", 0, &[1]).unwrap();
        }
        assert_eq!(vectors::count(&tables.lock()).unwrap(), 0);
        assert!(
            tables.lock().begin_page().is_ok(),
            "the page is closed and the tables take a new one"
        );
    }
}
