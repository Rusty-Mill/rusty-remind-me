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
//! The tables outside the core (revisions, the wiki, saved searches,
//! import archives, the sync log, analytics) are covered the same way: a
//! write to one inside the page puts the record it replaces in the undo
//! log first ([`EngineTables::note`]) and joins the page's redo batch.
//! Before this, a crash inside an update could keep its revision and lose
//! the row change (Q22, D62).
//!
//! While a page is open, only the thread that opened it may write to the
//! core; another thread's write is refused rather than folded into a page
//! it did not ask for.

use super::capture::{is_row_table, put_row, row_of, CapturedChange, WIKI_PAGES};
use super::core::{apply, decode, encode, Change, CoreTables};
use super::{engine_error, wiki, EngineTables};
use crate::db::{Result, StoreError};
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
    /// Its writes to the tables outside the core, in order.
    rows: Vec<CapturedChange>,
    /// The record each of those replaced, oldest first.
    rows_before: Vec<CapturedChange>,
    /// Set when a write failed part-way: the page can then only be
    /// abandoned.
    broken: bool,
    /// Set while the last noted write to a table outside the core has not
    /// yet landed ([`EngineTables::confirm_in_page`]). Still set at the
    /// next note or at finish, that write failed: its redo entry would
    /// replay a write the stores never took, so the page can only be
    /// abandoned.
    unconfirmed: bool,
}

/// The change that puts back what `change` is about to replace.
pub(super) fn before_image(core: &CoreTables, change: &Change) -> Change {
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
        Change::Reference(id, _) => Change::Reference(*id, core.references.get(*id).map(Box::new)),
        Change::Session(id, _) => Change::Session(*id, core.sessions.get(*id).map(Box::new)),
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
            rows: Vec::new(),
            rows_before: Vec::new(),
            broken: false,
            unconfirmed: false,
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
        if let Err(e) = self.undo.commit(&encode(&before)?) {
            // With a sink set, the page may already hold other tables'
            // writes: it can then only be abandoned, so none is published.
            if self.sink.is_some() {
                page.broken = true;
            }
            return Err(engine_error(e));
        }
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

    /// Note `change`, a write to a table outside the core, in the open
    /// page before it lands: `before`, the record it replaces, to the undo
    /// log, synced, and the write to the page's redo batch.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if no page is open or the undo log fails;
    /// the write must then not be made.
    pub(super) fn hold_in_page(
        &mut self,
        change: &CapturedChange,
        before: &CapturedChange,
    ) -> Result<()> {
        let Some(page) = self.page.as_mut() else {
            return Err(StoreError::Engine("no page is open".to_string()));
        };
        let mut undo = Batch::default();
        put_row(&mut undo, before);
        self.undo.commit(&undo).map_err(engine_error)?;
        page.broken |= page.unconfirmed;
        page.rows_before.push(before.clone());
        page.rows.push(change.clone());
        page.unconfirmed = true;
        Ok(())
    }

    /// The write noted last in this thread's page landed. Every write to a
    /// table outside the core confirms through [`EngineTables::emit`].
    pub(super) fn confirm_in_page(&mut self) {
        if let Some(page) = self.page.as_mut() {
            if page.owner == std::thread::current().id() {
                page.unconfirmed = false;
            }
        }
    }

    /// Refuse to commit this thread's open page: a write in it failed, and
    /// whatever its caller does with the error, the page can now only be
    /// abandoned.
    pub(super) fn break_page(&mut self) {
        if let Some(page) = self.page.as_mut() {
            if page.owner == std::thread::current().id() {
                page.broken = true;
            }
        }
    }

    /// Finish the open page: its writes become one redo batch.
    ///
    /// On failure the page is always closed, in one of the states
    /// `Backend::commit_transaction` allows: rolled back (a failed write,
    /// or a batch that never reached the journal), or needing recovery
    /// (`failed`): a journal error, whose outcome is indeterminate, or a
    /// checkpoint failure after the commit point. Reopening settles both
    /// from the undo log and the redo journal.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if no page is open, a write in it failed, or
    /// a journal fails.
    pub(crate) fn finish_page(&mut self) -> Result<()> {
        let Some(page) = self.page.take() else {
            return Err(StoreError::Engine("no page is open".to_string()));
        };
        if self.sink.is_some() && self.failed {
            // Something in the page could not be put back or handed over:
            // publish nothing. Reopening undoes the page from the undo log.
            self.drop_held();
            return Err(StoreError::Engine(
                "the tables need reopening; the page was not committed".to_string(),
            ));
        }
        if page.broken || page.unconfirmed {
            self.page = Some(page);
            self.abandon_page()?;
            return Err(StoreError::Engine(
                "a write in the page failed; the page was rolled back".to_string(),
            ));
        }
        let batch = match encode(&page.changes) {
            Ok(batch) => batch,
            Err(e) => {
                // Nothing reached the journal: definitely not durable, so
                // put the page back as reopening would.
                if self.put_back(page.before, page.rows_before).is_err() {
                    self.failed = true;
                }
                // A failed put-back sets `failed`; the encode error is
                // the one to report.
                let _ = self.undo_held(0);
                return Err(e);
            }
        };
        let mut redo = batch.clone();
        for row in &page.rows {
            put_row(&mut redo, row);
        }
        if let Err(e) = self.commit_redo(&redo) {
            // Indeterminate: the journal can fail after writing a whole
            // frame (its sync failed), so the batch may or may not replay.
            // Leave the stores and the undo log as they are and refuse
            // further use; reopening undoes the page from the undo log,
            // then replays the redo frame if it is whole.
            self.failed = true;
            self.drop_held();
            return Err(e);
        }
        // The page's other-table writes (held by the sink's staging) and
        // its core batch, as one unit.
        self.emit_batch(&batch)?;
        let checkpointed = self.checkpoint_after_commit();
        if checkpointed.is_err() {
            self.failed = true;
        }
        checkpointed
    }

    /// The commit point: the page's writes, core and not, encoded as
    /// `batch`, as one redo entry.
    fn commit_redo(&mut self, batch: &Batch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.journal.commit(batch).map_err(engine_error)
    }

    /// Housekeeping once the redo batch is durable.
    fn checkpoint_after_commit(&mut self) -> Result<()> {
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
        let rolled_back = self.put_back(page.before, page.rows_before);
        if rolled_back.is_err() {
            self.failed = true;
        }
        // With a sink set: the other tables' writes too, none handed over.
        let undone = self.undo_held(0);
        rolled_back.and(undone)
    }

    /// Put back what a page replaced, newest first: the tables outside
    /// the core, then the core.
    fn put_back(&mut self, before: Vec<Change>, rows: Vec<CapturedChange>) -> Result<()> {
        let wiki_pages = rows.iter().any(|r| r.table == WIKI_PAGES);
        for row in rows.into_iter().rev() {
            self.apply_row(&row)?;
        }
        if wiki_pages {
            self.wiki_search = wiki::index_pages(&self.wiki_pages);
        }
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
            for change in batch.changes.iter().rev() {
                if is_row_table(&change.store) {
                    self.apply_row(&row_of(change))?;
                    continue;
                }
                let single = Batch {
                    changes: vec![change.clone()],
                };
                for core in decode(&single)? {
                    apply(&mut self.core, core)?;
                }
            }
        }
        self.undo.checkpoint().map_err(engine_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine::{memories, reopen};
    use crate::db::memories::NewMemory;
    use crate::db::vectors::VectorStore;
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
        VectorStore::put(tables, "a", 0, &[1]).unwrap();
        tables.begin_page().unwrap();
        VectorStore::put(tables, "a", 0, &[2]).unwrap();
        memories::insert(tables, &NewMemory::new("m", "quokka", NOW)).unwrap();
    }

    fn landed(tables: &EngineTables) -> (Option<Vec<u8>>, bool, usize) {
        let core = &tables.core;
        (
            VectorStore::any_embedding(tables).unwrap(),
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
            VectorStore::put(&mut tables, "b", 0, &[3]).unwrap();
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

    fn tracked(content: &str) -> crate::db::history::Tracked {
        crate::db::history::Tracked {
            content: content.into(),
            category: "fact".into(),
            tags: "[]".into(),
            metadata: "{}".into(),
            sensitive: Some(false),
            project: Some(None),
        }
    }

    /// A revision of "old" stored before the page, then a page that adds a
    /// revision, deletes the old one, and writes a wiki page: every one a
    /// table outside the core (Q22, D62).
    fn write_rows_in_a_page(tables: &mut EngineTables) {
        use crate::db::history::HistoryStore;
        use crate::db::wiki::WikiStore;
        HistoryStore::insert(tables, "old", &tracked("before the page"), NOW, None).unwrap();
        tables.begin_page().unwrap();
        HistoryStore::insert(tables, "m", &tracked("in the page"), NOW, None).unwrap();
        HistoryStore::delete_for(tables, "old").unwrap();
        WikiStore::upsert_unbacked(tables, "wallaby", "Wallaby", "wallaby text", "", NOW).unwrap();
    }

    /// The new revision, the old one, the wiki page, and whether the
    /// wiki's search finds it.
    fn rows_landed(tables: &EngineTables) -> (usize, usize, bool, bool) {
        use crate::db::history::HistoryStore;
        use crate::db::wiki::WikiStore;
        (
            HistoryStore::list(tables, "m", 10).unwrap().len(),
            HistoryStore::list(tables, "old", 10).unwrap().len(),
            WikiStore::get(tables, "wallaby").unwrap().is_some(),
            !WikiStore::search(tables, &["wallaby".to_string()], 10)
                .unwrap()
                .is_empty(),
        )
    }

    #[test]
    fn a_crash_inside_a_page_puts_back_the_tables_outside_the_core() {
        let dir = fresh_dir("rows_crash_inside");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_rows_in_a_page(&mut tables);
            assert_eq!(rows_landed(&tables), (1, 0, true, true));
            // Dropped with the page open: the process died here.
        }
        let tables = reopen(&dir);
        assert_eq!(rows_landed(&tables), (0, 1, false, false));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_abandoned_page_puts_back_the_tables_outside_the_core() {
        let dir = fresh_dir("rows_abandoned");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_rows_in_a_page(&mut tables);
            tables.abandon_page().unwrap();
            assert_eq!(rows_landed(&tables), (0, 1, false, false));
        }
        let tables = reopen(&dir);
        assert_eq!(rows_landed(&tables), (0, 1, false, false));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_finished_page_keeps_the_tables_outside_the_core() {
        let dir = fresh_dir("rows_finished");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_rows_in_a_page(&mut tables);
            tables.finish_page().unwrap();
        }
        let tables = reopen(&dir);
        assert_eq!(rows_landed(&tables), (1, 0, true, true));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_after_the_redo_commit_keeps_the_tables_outside_the_core() {
        let dir = fresh_dir("rows_crash_after_commit");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            write_rows_in_a_page(&mut tables);
            // The redo batch is durable; the undo log was never emptied.
            let page = tables.page.as_ref().unwrap();
            let mut redo = encode(&page.changes).unwrap();
            for row in &page.rows {
                put_row(&mut redo, row);
            }
            tables.journal.commit(&redo).unwrap();
        }
        let tables = reopen(&dir);
        assert_eq!(rows_landed(&tables), (1, 0, true, true));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A page that deletes the revisions of "known".
    fn delete_known_in_a_page(tables: &mut EngineTables) {
        use crate::db::history::HistoryStore;
        tables.begin_page().unwrap();
        HistoryStore::delete_for(tables, "known").unwrap();
    }

    fn known_projects(tables: &EngineTables) -> Vec<Option<Option<String>>> {
        use crate::db::history::HistoryStore;
        HistoryStore::list(tables, "known", 10)
            .unwrap()
            .into_iter()
            .map(|r| r.project)
            .collect()
    }

    #[test]
    fn a_put_back_revision_keeps_its_known_empty_project() {
        let dir = fresh_dir("known_project");
        {
            use crate::db::history::HistoryStore;
            let mut tables = EngineTables::open(&dir).unwrap();
            // Its project was known to be none.
            HistoryStore::insert(&mut tables, "known", &tracked("no project"), NOW, None).unwrap();
            delete_known_in_a_page(&mut tables);
            tables.abandon_page().unwrap();
            assert_eq!(known_projects(&tables), vec![Some(None)]);
            delete_known_in_a_page(&mut tables);
            // Dropped with the page open: the process died here.
        }
        let tables = reopen(&dir);
        assert_eq!(known_projects(&tables), vec![Some(None)]);
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_noted_write_that_never_lands_cannot_be_committed() {
        use crate::db::wiki::WikiStore;
        let dir = fresh_dir("unlanded");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            tables.begin_page().unwrap();
            WikiStore::upsert_unbacked(&mut tables, "wallaby", "Wallaby", "text", "", NOW).unwrap();
            // Noted, then the write failed and its caller went on.
            tables
                .note(
                    super::super::capture::REVISIONS,
                    &99_i64,
                    Some(&"never landed"),
                )
                .unwrap();
            assert!(tables.finish_page().is_err());
            assert!(WikiStore::get(&tables, "wallaby").unwrap().is_none());
        }
        let tables = reopen(&dir);
        assert!(WikiStore::get(&tables, "wallaby").unwrap().is_none());
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_operation_cannot_be_committed_with_its_page() {
        use crate::db::wiki::WikiStore;
        let dir = fresh_dir("failed_operation");
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            tables.begin_page().unwrap();
            WikiStore::upsert_unbacked(&mut tables, "wallaby", "Wallaby", "text", "", NOW).unwrap();
            let failed: Result<()> = tables.logged(|_| Err(StoreError::Engine("boom".into())));
            assert!(failed.is_err());
            assert!(tables.finish_page().is_err());
            assert!(WikiStore::get(&tables, "wallaby").unwrap().is_none());
        }
        let tables = reopen(&dir);
        assert!(WikiStore::get(&tables, "wallaby").unwrap().is_none());
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
}
