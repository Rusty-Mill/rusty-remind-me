//! The whole route surface, exercised against a real engine store.
//!
//! The suite itself is `suite/routes.rs`. It goes through [`dispatch`]
//! rather than calling handlers directly, so auth, method matching and the
//! response envelope are covered by the same tests as the behaviour — the
//! three places a route can be wrong independently of its logic.
//!
//! [`dispatch`]: remind_me_hub::dispatch

use remind_me_hub::record::Record;
use remind_me_hub::store::{Counts, GraphPullQuery, HubStore, PullQuery, Stats, StoreResult};
use serde_json::Value;

/// A store for one test, dropped (and cleaned up) with it.
pub struct TestStore {
    store: Box<dyn HubStore>,
    /// A data directory to remove on drop, for a backend that has one.
    dir: Option<std::path::PathBuf>,
}

/// Forwarded whole, so the suite passes `&store` wherever it passes a
/// `&dyn HubStore` — a `Deref` alone would not do it, because the unsizing
/// coercion to `&dyn HubStore` is tried before a deref.
impl HubStore for TestStore {
    fn ping(&self) -> StoreResult<()> {
        self.store.ping()
    }
    fn apply_record(&self, record: &Record, origin: Option<&str>) -> StoreResult<bool> {
        self.store.apply_record(record, origin)
    }
    fn apply_records(&self, records: &[Record], origin: Option<&str>) -> Vec<StoreResult<bool>> {
        self.store.apply_records(records, origin)
    }
    fn stats(&self) -> StoreResult<Stats> {
        self.store.stats()
    }
    fn count_tables(&self, wanted: &[&str]) -> StoreResult<Counts> {
        self.store.count_tables(wanted)
    }
    fn approx_count_tables(&self, wanted: &[&str]) -> StoreResult<Option<Counts>> {
        self.store.approx_count_tables(wanted)
    }
    fn count_tables_since(&self, wanted: &[&str], since: &str) -> StoreResult<Counts> {
        self.store.count_tables_since(wanted, since)
    }
    fn count_by_origin_node(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        self.store.count_by_origin_node(since)
    }
    fn count_by_category(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        self.store.count_by_category(since)
    }
    fn compact_tombstones(&self, cutoff: &str) -> StoreResult<usize> {
        self.store.compact_tombstones(cutoff)
    }
    fn pull_memories(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        self.store.pull_memories(query)
    }
    fn pull_entities(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        self.store.pull_entities(query)
    }
    fn pull_links(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        self.store.pull_links(query)
    }
    fn pull_entity_relations(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        self.store.pull_entity_relations(query)
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

// `#[path = "suite"]` makes the inline module's own directory `suite/`, so
// `routes` resolves to `suite/routes.rs`.
#[path = "suite"]
mod engine {
    use super::TestStore;
    use remind_me_hub::store::multimodal::MultimodalHubStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> TestStore {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remind_me_hub_routes_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = MultimodalHubStore::open(&dir).expect("open a hub data directory");
        TestStore {
            store: Box::new(store),
            dir: Some(dir),
        }
    }

    mod routes;
}
