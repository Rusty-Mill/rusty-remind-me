//! The change hook (D48): every write the engine makes durable, handed to
//! a [`ChangeSink`] as table, key and record. R3's Attest adapter uses it
//! to keep Attest's log as the durable store under engine tables held in
//! RAM, and replays the log with [`EngineTables::apply_captured`].
//!
//! Hidden test and benchmark support: production sets no sink, and then
//! nothing is serialised and nothing below changes how the engine behaves.
//!
//! With a sink set, the sink sees whole operations, never part of one:
//!
//! - Inside a page (a transaction), every write, core or not, is held and
//!   handed over with the page's core batch as one unit when the page
//!   finishes. Abandoning the page hands over nothing and puts back the
//!   other tables' records it replaced, as the page does for the core.
//! - Outside a page, a write to the other tables is handed over as soon as
//!   it lands, or, inside a multi-row operation ([`EngineTables::logged`]),
//!   when the operation succeeds. An operation that fails puts its records
//!   back and hands over nothing.
//! - Sequence ids are reserved in blocks of [`RESERVED_IDS`]: before a
//!   sequence issues an id past its last reservation, the next block's
//!   high-water mark is handed over on its own, as a unit with no rows,
//!   and the id is issued only once the sink holds it. A replay raises
//!   each sequence past every reservation, so no id is issued twice, even
//!   one whose record was deleted or never committed. A failed reservation
//!   issues nothing.
//! - A sink that fails, or records that cannot be put back, leave the
//!   tables refusing every write until reopened (`failed`): their RAM state
//!   may then be ahead of the sink.

use super::core::decode;
use super::{deleted, engine_error, wiki, EngineTables};
use crate::db::{Result, StoreError};
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::query::GetById;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use rusty_multimodal_db_engine::journal::Batch;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub(crate) const SAVED_SEARCHES: &str = "saved_searches";
pub(crate) const SEEN: &str = "saved_search_seen";
pub(crate) const ARCHIVES: &str = "import_archives";
pub(crate) const SPANS: &str = "import_archive_spans";
pub(crate) const SYNC_LOG: &str = "sync_log";
pub(crate) const SNAPSHOTS: &str = super::analytics::SEQUENCE;
pub(crate) const REVISIONS: &str = super::revisions::SEQUENCE;
pub(crate) const WIKI_PAGES: &str = "wiki_pages";
pub(crate) const WIKI_LINKS: &str = "wiki_links";
pub(crate) const WIKI_META: &str = "wiki_meta";
/// Not a table: an id a sequence issued, kept so a replay never reissues
/// it, even after its record is deleted.
pub(crate) const SEQUENCES: &str = "sequence";
/// How many ids one reservation covers: one extra unit per this many.
pub(crate) const RESERVED_IDS: i64 = 64;

/// One write: the table, the record's key as JSON, and the record as JSON
/// (`None` for a delete).
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedChange {
    pub table: String,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Where the engine hands each durable write (D48).
#[doc(hidden)]
pub trait ChangeSink: Send {
    /// Make `changes` durable as one unit before returning.
    ///
    /// # Errors
    ///
    /// The sink's error; the engine then refuses writes until reopened.
    fn committed(&mut self, changes: Vec<CapturedChange>) -> Result<()>;
}

/// A noted write, with the record it replaces (`None` for a core write,
/// which the page's undo log covers).
#[derive(Debug)]
pub(crate) struct Noted {
    change: CapturedChange,
    before: Option<CapturedChange>,
}

/// Writes held until their operation or page ends.
#[derive(Debug, Default)]
pub(crate) struct Staging {
    held: Vec<Noted>,
    /// How many [`EngineTables::logged`] operations are running.
    depth: u32,
    /// The highest id the sink holds a reservation for, by sequence.
    reserved: std::collections::BTreeMap<String, i64>,
}

/// `table`'s write of `record` under `key`.
fn change<K: Serialize, R: Serialize>(
    table: &str,
    key: &K,
    record: Option<&R>,
) -> Result<CapturedChange> {
    Ok(CapturedChange {
        table: table.to_string(),
        key: serde_json::to_vec(key).map_err(engine_error)?,
        value: record
            .map(|r| serde_json::to_vec(r).map_err(engine_error))
            .transpose()?,
    })
}

impl EngineTables {
    /// Hand every later durable write to `sink`, or to none.
    #[doc(hidden)]
    pub fn set_change_sink(&mut self, sink: Option<Box<dyn ChangeSink>>) {
        self.sink = sink;
    }

    /// The write `table` is about to make. Call it before the write.
    ///
    /// Inside this thread's page it always goes to the page: the record it
    /// replaces to the undo log, synced, and the write to the page's redo
    /// batch, so a crash or an abandoned page puts it back (Q22, D62).
    /// It is returned for the sink when one is set: with a sink, it refuses
    /// once the tables need reopening, or while another thread holds a
    /// page open.
    pub(crate) fn note<K: Serialize, R: Serialize>(
        &mut self,
        table: &str,
        key: &K,
        record: Option<&R>,
    ) -> Result<Option<Noted>> {
        let paged = matches!(self.in_page(), Ok(true));
        if self.sink.is_none() && !paged {
            return Ok(None);
        }
        self.ensure_writable()?;
        if self.sink.is_some() {
            self.in_page()?;
        }
        let change = change(table, key, record)?;
        let before = CapturedChange {
            value: self.stored(table, &change.key)?,
            ..change.clone()
        };
        if paged {
            self.hold_in_page(&change, &before)?;
        }
        if self.sink.is_none() {
            return Ok(None);
        }
        Ok(Some(Noted {
            change,
            before: Some(before),
        }))
    }

    /// Hold a noted write, and hand it over unless its operation or page
    /// is still running.
    pub(crate) fn emit(&mut self, noted: Option<Noted>) -> Result<()> {
        let Some(noted) = noted else {
            return Ok(());
        };
        self.staging.held.push(noted);
        self.flush_unless_held()
    }

    /// Hand a core batch to the sink with whatever its page held: called
    /// once the batch is committed.
    pub(crate) fn emit_batch(&mut self, batch: &Batch) -> Result<()> {
        if self.sink.is_none() {
            return Ok(());
        }
        let core = batch.changes.iter().map(|c| Noted {
            change: CapturedChange {
                table: c.store.clone(),
                key: c.key.clone(),
                value: c.value.clone(),
            },
            before: None,
        });
        self.staging.held.extend(core);
        self.flush_unless_held()
    }

    /// Make sure the sink holds a reservation covering `id`, about to be
    /// issued by sequence `name`, before it is: past the last one, hand
    /// over the next block's high-water mark as a unit of its own.
    ///
    /// # Errors
    ///
    /// The sink's error; the tables then refuse writes and `id` must not
    /// be issued.
    pub(crate) fn reserve(&mut self, name: &str, id: i64) -> Result<()> {
        if self.sink.is_none() {
            return Ok(());
        }
        if self
            .staging
            .reserved
            .get(name)
            .is_some_and(|&top| id <= top)
        {
            return Ok(());
        }
        self.ensure_writable()?;
        let top = id.saturating_add(RESERVED_IDS - 1);
        self.hand_over(vec![change(SEQUENCES, &name, Some(&top))?])?;
        self.staging.reserved.insert(name.to_string(), top);
        Ok(())
    }

    /// Run `write` as one operation: what it writes reaches the sink as
    /// one unit when it succeeds (or with the open page); when it fails,
    /// its records are put back and nothing reaches the sink. For writes
    /// to the tables outside the core only.
    pub(crate) fn logged<T>(&mut self, write: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.sink.is_none() {
            return write(self);
        }
        let mark = self.staging.held.len();
        self.staging.depth += 1;
        let written = write(self);
        self.staging.depth -= 1;
        if written.is_err() {
            // A failed put-back sets `failed`; the write's own error is
            // the one to report.
            let _ = self.undo_held(mark);
            return written;
        }
        self.flush_unless_held()?;
        written
    }

    /// Put back what the writes held since `mark` replaced, newest first,
    /// and drop them.
    ///
    /// # Errors
    ///
    /// A record that could not be put back; the tables then refuse writes.
    pub(crate) fn undo_held(&mut self, mark: usize) -> Result<()> {
        let undone: Vec<Noted> = self.staging.held.drain(mark..).collect();
        let mut wiki_pages = false;
        let mut put_back = Ok(());
        for noted in undone.into_iter().rev() {
            let Some(before) = noted.before else {
                continue;
            };
            wiki_pages |= before.table == WIKI_PAGES;
            if let Err(e) = self.apply_row(&before) {
                self.failed = true;
                put_back = put_back.and(Err(e));
            }
        }
        if wiki_pages {
            self.wiki_search = wiki::index_pages(&self.wiki_pages);
        }
        put_back
    }

    /// Drop every held write: their outcome is settled by reopening.
    pub(crate) fn drop_held(&mut self) {
        self.staging.held.clear();
    }

    fn flush_unless_held(&mut self) -> Result<()> {
        if self.page.is_some() || self.staging.depth > 0 || self.staging.held.is_empty() {
            return Ok(());
        }
        let changes = std::mem::take(&mut self.staging.held)
            .into_iter()
            .map(|n| n.change)
            .collect();
        self.hand_over(changes)
    }

    fn hand_over(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
        let Some(sink) = self.sink.as_mut() else {
            return Ok(());
        };
        let handed = sink.committed(changes);
        if handed.is_err() {
            self.failed = true;
        }
        handed
    }

    /// The record `table` holds under `key`, as JSON, if any.
    fn stored(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match table {
            SAVED_SEARCHES => read(&self.saved_searches, key),
            SEEN => read(&self.seen, key),
            ARCHIVES => read(&self.archives, key),
            SPANS => read(&self.spans, key),
            SYNC_LOG => read(&self.sync_log, key),
            SNAPSHOTS => read(&self.snapshots, key),
            REVISIONS => read(&self.revisions, key),
            WIKI_PAGES => read(&self.wiki_pages, key),
            WIKI_LINKS => read(&self.wiki_links, key),
            WIKI_META => read(&self.wiki_meta, key),
            other => Err(StoreError::Engine(format!("no captured table {other}"))),
        }
    }

    /// Apply writes a sink was handed, in order: core writes as one
    /// journal batch, the rest table by table. Raises the id sequences
    /// past every replayed id and rebuilds the wiki's search index. Call
    /// it with no sink set, or the writes are handed over again.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] for a table this build does not know, a
    /// record it cannot read, or a failed write.
    #[doc(hidden)]
    pub fn apply_captured(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
        let mut core = Batch::default();
        for c in changes {
            if !self.apply_row(&c)? {
                match c.value {
                    Some(value) => core.put(&c.table, c.key, value),
                    None => core.delete(&c.table, c.key),
                };
            }
        }
        self.commit(decode(&core)?)?;
        self.raise_sequences();
        self.wiki_search = wiki::index_pages(&self.wiki_pages);
        Ok(())
    }

    /// Raise the sequence `c` names past the id it issued.
    fn replay_sequence(&mut self, c: &CapturedChange) -> Result<()> {
        let name: String = serde_json::from_slice(&c.key).map_err(engine_error)?;
        let value = c.value.as_deref().unwrap_or_default();
        let id: i64 = serde_json::from_slice(value).map_err(engine_error)?;
        let floor = u64::try_from(id).map_err(engine_error)?;
        self.journal.raise_to(&name, floor);
        Ok(())
    }

    /// Apply `c` if it is to a table outside the core; `false` if not.
    pub(super) fn apply_row(&mut self, c: &CapturedChange) -> Result<bool> {
        match c.table.as_str() {
            SAVED_SEARCHES => replay(&mut self.saved_searches, c)?,
            SEEN => replay(&mut self.seen, c)?,
            ARCHIVES => replay(&mut self.archives, c)?,
            SPANS => replay(&mut self.spans, c)?,
            SYNC_LOG => replay(&mut self.sync_log, c)?,
            SNAPSHOTS => replay(&mut self.snapshots, c)?,
            REVISIONS => replay(&mut self.revisions, c)?,
            WIKI_PAGES => replay(&mut self.wiki_pages, c)?,
            WIKI_LINKS => replay(&mut self.wiki_links, c)?,
            WIKI_META => replay(&mut self.wiki_meta, c)?,
            SEQUENCES => self.replay_sequence(c)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// Whether `store` is one of the tables outside the core, which a page
/// covers through [`EngineTables::note`].
pub(super) fn is_row_table(store: &str) -> bool {
    matches!(
        store,
        SAVED_SEARCHES
            | SEEN
            | ARCHIVES
            | SPANS
            | SYNC_LOG
            | SNAPSHOTS
            | REVISIONS
            | WIKI_PAGES
            | WIKI_LINKS
            | WIKI_META
    )
}

/// `c` as a write in `batch`.
pub(super) fn put_row(batch: &mut Batch, c: &CapturedChange) {
    match &c.value {
        Some(value) => batch.put(&c.table, c.key.clone(), value.clone()),
        None => batch.delete(&c.table, c.key.clone()),
    };
}

/// A batch write to a table outside the core, as a captured change.
pub(super) fn row_of(change: &rusty_multimodal_db_engine::journal::Change) -> CapturedChange {
    CapturedChange {
        table: change.store.clone(),
        key: change.key.clone(),
        value: change.value.clone(),
    }
}

/// `table`'s record under the JSON `key`, as JSON.
fn read<R, Index, Slot>(
    table: &GenericMmapStore<R, Index, Slot>,
    key: &[u8],
) -> Result<Option<Vec<u8>>>
where
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned + std::fmt::Debug,
    R::ScanValue: MmapFieldValue,
{
    let id: R::Id = serde_json::from_slice(key).map_err(engine_error)?;
    table
        .get(id)
        .map(|r| serde_json::to_vec(&r).map_err(engine_error))
        .transpose()
}

/// Put or delete one record of `table` as `c` says.
fn replay<R, Index, Slot>(
    table: &mut GenericMmapStore<R, Index, Slot>,
    c: &CapturedChange,
) -> Result<()>
where
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned + std::fmt::Debug,
    R::ScanValue: MmapFieldValue,
{
    let id: R::Id = serde_json::from_slice(&c.key).map_err(engine_error)?;
    let Some(value) = &c.value else {
        return deleted(table.delete(id));
    };
    let record: R = serde_json::from_slice(value)
        .map_err(|e| StoreError::Engine(format!("{}: {e}", c.table)))?;
    if table.get(id).is_some() {
        table.replace(record).map_err(engine_error)
    } else {
        table.insert(record).map_err(engine_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::history::{HistoryStore, Tracked};
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::db::saved_searches::SavedSearchStore;
    use crate::db::wiki::WikiStore;
    use crate::models::SavedSearch;
    use std::sync::{Arc, Mutex};

    const NOW: &str = "2026-11-21T00:00:00+00:00";

    type Log = Arc<Mutex<Vec<Vec<CapturedChange>>>>;

    /// Collects every hand-over, one entry per unit.
    struct Collect(Log);

    impl ChangeSink for Collect {
        fn committed(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
            self.0.lock().unwrap().push(changes);
            Ok(())
        }
    }

    fn collecting() -> (EngineTables, Log) {
        let log = Log::default();
        let mut t = EngineTables::open_temporary().unwrap();
        t.set_change_sink(Some(Box::new(Collect(log.clone()))));
        (t, log)
    }

    /// Fresh tables holding only what the sink was handed.
    fn rebuilt(log: &Log) -> EngineTables {
        let mut t = EngineTables::open_temporary().unwrap();
        t.apply_captured(log.lock().unwrap().concat()).unwrap();
        t
    }

    /// The rows, not issued ids, in the units after the first `skip`.
    fn rows_after(log: &Log, skip: usize) -> Vec<CapturedChange> {
        log.lock().unwrap()[skip..]
            .concat()
            .into_iter()
            .filter(|c| c.table != SEQUENCES)
            .collect()
    }

    fn tables_of(unit: &[CapturedChange]) -> Vec<&str> {
        unit.iter().map(|c| c.table.as_str()).collect()
    }

    /// Refuses every hand-over.
    struct Refuse;

    impl ChangeSink for Refuse {
        fn committed(&mut self, _: Vec<CapturedChange>) -> Result<()> {
            Err(StoreError::Engine("full".into()))
        }
    }

    fn tracked() -> Tracked {
        Tracked {
            content: "x".into(),
            category: "fact".into(),
            tags: String::new(),
            metadata: "{}".into(),
            sensitive: Some(false),
            project: Some(None),
        }
    }

    /// Writes to core and other tables, replayed on fresh tables, read
    /// back the same, with the id sequences past the replayed ids.
    #[test]
    fn captured_writes_replay_onto_fresh_tables() {
        let (mut a, log) = collecting();
        MemoryStore::insert(&mut a, &NewMemory::new("m", "quokka", NOW)).unwrap();
        MemoryStore::insert(&mut a, &NewMemory::new("gone", "x", NOW)).unwrap();
        MemoryStore::delete_live(&mut a, "gone", None).unwrap();
        let saved = SavedSearch {
            id: "s".into(),
            name: "quokkas".into(),
            query: "quokka".into(),
            filters: Default::default(),
            watch: false,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        };
        SavedSearchStore::insert(&mut a, &saved).unwrap();
        HistoryStore::insert(&mut a, "m", &tracked(), NOW, None).unwrap();
        WikiStore::set_meta(&mut a, "k", "v").unwrap();

        let mut b = rebuilt(&log);
        assert!(MemoryStore::exists(&b, "m").unwrap());
        assert!(!MemoryStore::exists(&b, "gone").unwrap());
        assert_eq!(SavedSearchStore::get(&b, "s").unwrap().name, "quokkas");
        assert_eq!(WikiStore::meta(&b, "k").unwrap().as_deref(), Some("v"));
        HistoryStore::insert(&mut b, "m", &tracked(), NOW, None).unwrap();
        let ids: Vec<i64> = HistoryStore::list(&b, "m", 10)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids.len(), 2, "the replayed id was not reissued: {ids:?}");
    }

    #[test]
    fn no_sink_notes_nothing_and_a_failing_sink_fails_the_tables_closed() {
        let mut t = EngineTables::open_temporary().unwrap();
        assert!(t.note(SEEN, &1, Some(&1)).unwrap().is_none());
        t.set_change_sink(Some(Box::new(Refuse)));
        assert!(MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).is_err());
        t.set_change_sink(None);
        assert!(
            MemoryStore::insert(&mut t, &NewMemory::new("n", "x", NOW)).is_err(),
            "refuses writes until reopened"
        );
    }

    /// A transaction's revision and row change reach the sink as one unit
    /// when it commits, and rebuild together from the log alone.
    #[test]
    fn a_committed_mixed_page_is_handed_over_once() {
        let (mut t, log) = collecting();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("m", "quokka", NOW)).unwrap();
        assert!(rows_after(&log, 0).is_empty(), "no row before the commit");
        let before = log.lock().unwrap().len();
        t.finish_page().unwrap();

        let units = log.lock().unwrap()[before..].to_vec();
        assert_eq!(units.len(), 1, "one unit: {units:?}");
        assert!(tables_of(&units[0]).contains(&REVISIONS));
        let b = rebuilt(&log);
        assert!(MemoryStore::exists(&b, "m").unwrap());
        assert_eq!(HistoryStore::list(&b, "m", 10).unwrap().len(), 1);
    }

    /// An abandoned transaction hands over nothing and puts back the
    /// other tables' records it wrote, as it does the core's.
    #[test]
    fn an_abandoned_mixed_page_hands_over_nothing_and_puts_back() {
        let (mut t, log) = collecting();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        WikiStore::set_meta(&mut t, "k", "old").unwrap();
        let before = log.lock().unwrap().len();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        HistoryStore::delete_for(&mut t, "m").unwrap();
        WikiStore::set_meta(&mut t, "k", "new").unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("m", "quokka", NOW)).unwrap();
        t.abandon_page().unwrap();

        assert!(rows_after(&log, before).is_empty(), "no rows handed over");
        assert!(!MemoryStore::exists(&t, "m").unwrap());
        assert_eq!(HistoryStore::list(&t, "m", 10).unwrap().len(), 1);
        assert_eq!(WikiStore::meta(&t, "k").unwrap().as_deref(), Some("old"));
        let b = rebuilt(&log);
        assert_eq!(HistoryStore::list(&b, "m", 10).unwrap().len(), 1);
        assert_eq!(WikiStore::meta(&b, "k").unwrap().as_deref(), Some("old"));
    }

    /// A multi-row write outside a transaction is one unit, not one per row.
    #[test]
    fn a_multi_row_operation_is_handed_over_once() {
        let (mut t, log) = collecting();
        let ids: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        SavedSearchStore::mark_seen(&mut t, "s", &ids, NOW).unwrap();
        let units = log.lock().unwrap().clone();
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(tables_of(&units[0]), [SEEN; 3]);
    }

    /// Once the tables need reopening, a held handle can neither write to
    /// the other tables nor commit its open transaction, and the sink gets
    /// nothing more.
    #[test]
    fn tables_that_need_reopening_refuse_writes_and_commits() {
        let (mut t, log) = collecting();
        t.begin_page().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("m", "quokka", NOW)).unwrap();
        t.failed = true;
        assert!(WikiStore::set_meta(&mut t, "k", "v").is_err());
        assert!(HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).is_err());
        assert!(t.finish_page().is_err());
        assert!(log.lock().unwrap().is_empty());
    }

    /// Deleting the highest revision before a rebuild from the log alone
    /// does not let its id be issued again.
    #[test]
    fn a_rebuild_never_reissues_a_deleted_top_id() {
        let (mut t, log) = collecting();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        let top = HistoryStore::list(&t, "m", 1).unwrap()[0].id;
        HistoryStore::delete_for(&mut t, "m").unwrap();

        let mut b = rebuilt(&log);
        HistoryStore::insert(&mut b, "m", &tracked(), NOW, None).unwrap();
        let next = HistoryStore::list(&b, "m", 1).unwrap()[0].id;
        assert!(next > top, "{next} reissues {top} or lower");
    }

    /// Collects hand-overs until told to fail.
    struct Flaky(Log, Arc<std::sync::atomic::AtomicBool>);

    impl ChangeSink for Flaky {
        fn committed(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(StoreError::Engine("full".into()));
            }
            self.0.lock().unwrap().push(changes);
            Ok(())
        }
    }

    /// After a failed hand-over the tables refuse writes, and tables
    /// rebuilt from what the sink holds have every earlier unit and none
    /// of the failed one.
    #[test]
    fn a_failed_hand_over_recovers_from_the_log() {
        let log = Log::default();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut t = EngineTables::open_temporary().unwrap();
        t.set_change_sink(Some(Box::new(Flaky(log.clone(), fail.clone()))));
        MemoryStore::insert(&mut t, &NewMemory::new("kept", "x", NOW)).unwrap();
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        t.begin_page().unwrap();
        WikiStore::set_meta(&mut t, "lost", "v").unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("lost", "x", NOW)).unwrap();
        assert!(t.finish_page().is_err());
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            WikiStore::set_meta(&mut t, "k", "v").is_err(),
            "fails closed"
        );

        let b = rebuilt(&log);
        assert!(MemoryStore::exists(&b, "kept").unwrap());
        assert!(!MemoryStore::exists(&b, "lost").unwrap());
        assert!(WikiStore::meta(&b, "lost").unwrap().is_none());
    }

    /// An id is issued only once the sink holds its reservation: a failed
    /// reservation issues nothing, and the tables then refuse writes.
    #[test]
    fn a_failed_reservation_issues_no_id() {
        let log = Log::default();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut t = EngineTables::open_temporary().unwrap();
        t.set_change_sink(Some(Box::new(Flaky(log.clone(), fail.clone()))));
        assert!(HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).is_err());
        assert!(HistoryStore::list(&t, "m", 10).unwrap().is_empty());
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            WikiStore::set_meta(&mut t, "k", "v").is_err(),
            "fails closed"
        );
        assert!(log.lock().unwrap().is_empty());
    }

    /// One reservation covers a block of ids: issuing them hands over one
    /// unit of its own, with no rows, ahead of the rows that use them.
    #[test]
    fn ids_are_reserved_in_blocks() {
        let (mut t, log) = collecting();
        for _ in 0..3 {
            HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        }
        let units = log.lock().unwrap().clone();
        let reservations: Vec<_> = units
            .iter()
            .filter(|u| u.iter().all(|c| c.table == SEQUENCES))
            .collect();
        assert_eq!(reservations.len(), 1, "{units:?}");
        assert_eq!(units.len(), 4, "one reservation, then three revisions");
    }

    /// An undo-log failure inside a page, even one the caller handles,
    /// publishes nothing: finishing the page fails, and the revision the
    /// page wrote before it does not reach the sink.
    #[test]
    fn an_undo_failure_in_a_page_publishes_nothing() {
        use rusty_multimodal_db_engine::test_support::{arm_fault, Fault};
        let (mut t, log) = collecting();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        arm_fault(Fault::JournalSync);
        assert!(MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).is_err());
        assert!(t.finish_page().is_err(), "no false success");
        assert!(rows_after(&log, 0).is_empty(), "no partial unit");
        assert!(HistoryStore::list(&t, "m", 10).unwrap().is_empty());
    }

    /// An id issued inside an abandoned page is not issued again after a
    /// rebuild, once a later unit reaches the sink.
    #[test]
    fn an_abandoned_page_hands_over_its_issued_ids_at_once() {
        let (mut t, log) = collecting();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        let abandoned = HistoryStore::list(&t, "m", 1).unwrap()[0].id;
        t.abandon_page().unwrap();
        let units = log.lock().unwrap().clone();
        assert!(
            units.concat().iter().all(|c| c.table == SEQUENCES),
            "only issued ids: {units:?}"
        );

        let mut b = rebuilt(&log);
        HistoryStore::insert(&mut b, "m", &tracked(), NOW, None).unwrap();
        let next = HistoryStore::list(&b, "m", 1).unwrap()[0].id;
        assert!(next > abandoned, "{next} reissues {abandoned} or lower");
    }

    /// A commit whose outcome is indeterminate still hands over the ids
    /// the page issued, and none of its rows.
    #[test]
    fn an_indeterminate_commit_hands_over_only_its_issued_ids() {
        use rusty_multimodal_db_engine::test_support::{arm_fault, Fault};
        let (mut t, log) = collecting();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        let issued = HistoryStore::list(&t, "m", 1).unwrap()[0].id;
        MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).unwrap();
        arm_fault(Fault::JournalSync);
        assert!(t.finish_page().is_err());
        assert!(log
            .lock()
            .unwrap()
            .concat()
            .iter()
            .all(|c| c.table == SEQUENCES));

        let mut b = rebuilt(&log);
        assert!(!MemoryStore::exists(&b, "m").unwrap());
        HistoryStore::insert(&mut b, "m", &tracked(), NOW, None).unwrap();
        let next = HistoryStore::list(&b, "m", 1).unwrap()[0].id;
        assert!(next > issued, "{next} reissues {issued} or lower");
    }

    #[test]
    fn an_abandoned_page_keeps_its_issued_ids() {
        let (mut t, log) = collecting();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        t.begin_page().unwrap();
        HistoryStore::insert(&mut t, "m", &tracked(), NOW, None).unwrap();
        let abandoned = HistoryStore::list(&t, "m", 1).unwrap()[0].id;
        t.abandon_page().unwrap();
        WikiStore::set_meta(&mut t, "k", "v").unwrap();

        let mut b = rebuilt(&log);
        HistoryStore::insert(&mut b, "m", &tracked(), NOW, None).unwrap();
        let next = HistoryStore::list(&b, "m", 1).unwrap()[0].id;
        assert!(next > abandoned, "{next} reissues {abandoned} or lower");
    }
}
