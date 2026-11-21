//! The change hook (D48): every write the engine makes durable, handed to
//! a [`ChangeSink`] as table, key and record. R3's Attest adapter uses it
//! to keep Attest's log as the durable store under engine tables held in
//! RAM, and replays the log with [`EngineTables::apply_captured`].
//!
//! Hidden test and benchmark support: production sets no sink, and then
//! nothing is serialised. Core writes are handed over once their journal
//! batch (or their page) is committed; the other tables' writes, which the
//! engine makes durable at once, right after each write. A sink that fails
//! leaves the tables refusing writes until reopened, as a batch that did
//! not reach every store does.

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

    /// The write `table` is about to make, when a sink wants it.
    pub(crate) fn note<K: Serialize, R: Serialize>(
        &self,
        table: &str,
        key: &K,
        record: Option<&R>,
    ) -> Result<Option<CapturedChange>> {
        if self.sink.is_none() {
            return Ok(None);
        }
        change(table, key, record).map(Some)
    }

    /// Hand a noted write to the sink.
    pub(crate) fn emit(&mut self, noted: Option<CapturedChange>) -> Result<()> {
        match noted {
            Some(change) => self.hand_over(vec![change]),
            None => Ok(()),
        }
    }

    /// Hand a committed core batch to the sink.
    pub(crate) fn emit_batch(&mut self, batch: &Batch) -> Result<()> {
        if self.sink.is_none() {
            return Ok(());
        }
        let changes = batch
            .changes
            .iter()
            .map(|c| CapturedChange {
                table: c.store.clone(),
                key: c.key.clone(),
                value: c.value.clone(),
            })
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

    /// Apply `c` if it is to a table outside the core; `false` if not.
    fn apply_row(&mut self, c: &CapturedChange) -> Result<bool> {
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
            _ => return Ok(false),
        }
        Ok(true)
    }
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

    /// Collects every hand-over.
    struct Collect(Arc<Mutex<Vec<CapturedChange>>>);

    impl ChangeSink for Collect {
        fn committed(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
            self.0.lock().unwrap().extend(changes);
            Ok(())
        }
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
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut a = EngineTables::open_temporary().unwrap();
        a.set_change_sink(Some(Box::new(Collect(log.clone()))));
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

        let mut b = EngineTables::open_temporary().unwrap();
        b.apply_captured(log.lock().unwrap().clone()).unwrap();
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
        assert_eq!(t.note(SEEN, &1, Some(&1)).unwrap(), None);
        t.set_change_sink(Some(Box::new(Refuse)));
        assert!(MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).is_err());
        t.set_change_sink(None);
        assert!(
            MemoryStore::insert(&mut t, &NewMemory::new("n", "x", NOW)).is_err(),
            "refuses writes until reopened"
        );
    }
}
