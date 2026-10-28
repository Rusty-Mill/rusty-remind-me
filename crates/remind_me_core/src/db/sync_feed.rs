//! Storage reads for the peer sync server: the four pull feeds (memories,
//! entities, mention links and relations), each keyset-paged, as the wire
//! records a pulling node applies, plus the graph table counts `/count`
//! reports.
//!
//! Every read [`crate::sync::server`] makes goes through here, onto the
//! engine's memories core (`db::engine::memories`, `db::engine::graph`).
//! The server keeps the rules: parameter defaults, the page limit, and the
//! response shapes.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use serde_json::Value;

/// The pull feeds, on the engine's memories core.
pub struct SyncFeed<'c> {
    core: &'c EngineLock,
}

impl<'c> SyncFeed<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Up to `limit` memories after the cursor `(since, since_id)` on
    /// `(updated_at, id)`, oldest first, skipping those `exclude_node` wrote
    /// when given.
    pub fn memories_after(
        &self,
        since: &str,
        since_id: &str,
        exclude_node: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        engine::memories::memories_after(&self.core.lock(), since, since_id, exclude_node, limit)
    }

    /// Up to `limit` entities after the cursor `(since, since_id)` on
    /// `(updated_at, id)`, oldest first, skipping those `exclude_node` wrote
    /// when given.
    pub fn entities_after(
        &self,
        since: &str,
        since_id: &str,
        exclude_node: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        engine::graph::entities_after(&self.core.lock(), since, since_id, exclude_node, limit)
    }

    /// Up to `limit` mention links after the cursor `(since, since_id)` on
    /// `(created_at, memory_id|entity_id)`, oldest first. Links have no
    /// `node_id`, so there is nothing to exclude.
    pub fn links_after(&self, since: &str, since_id: &str, limit: usize) -> Result<Vec<Value>> {
        engine::graph::links_after(&self.core.lock(), since, since_id, limit)
    }

    /// Up to `limit` relations after the cursor `(since, since_id)` on
    /// `(created_at, id)`, oldest first.
    pub fn relations_after(&self, since: &str, since_id: &str, limit: usize) -> Result<Vec<Value>> {
        engine::graph::relations_after(&self.core.lock(), since, since_id, limit)
    }

    /// How many entities are stored.
    pub fn entity_count(&self) -> Result<i64> {
        Ok(engine::graph::counts(&self.core.lock())?.0)
    }

    /// How many mention links are stored.
    pub fn link_count(&self) -> Result<i64> {
        Ok(engine::graph::counts(&self.core.lock())?.1)
    }

    /// How many relations are stored.
    pub fn relation_count(&self) -> Result<i64> {
        Ok(engine::graph::counts(&self.core.lock())?.2)
    }
}
