//! Where a write came from, which decides whether the sync outbox hears
//! about it.
//!
//! Up to schema v30, fifteen SQLite triggers kept the derived data (the
//! full-text index, the tag index and the outbox) in step with every write.
//! They could not tell a local edit from a record a peer had sent, so every
//! synced write queued an outbox row that then had to be found and marked
//! sent again. The repositories do that work now, on the engine
//! (`db::engine::memories`, `db::engine::graph`), with the write's
//! [`Origin`] deciding whether the outbox hears about it.

/// Where a write came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Made on this node. Queued for sync, when sync is enabled.
    Local,
    /// Applied from a record a peer or the hub sent. Never queued: sending
    /// it back would make two nodes chase each other.
    Sync,
}

/// Rebuild the engine's derived indexes (the memories' full-text and tag
/// indexes) from the stored rows. Everything here is derived, so a rebuild
/// is always safe; it is how rows planted by a test fixture, or an index
/// that was lost, become searchable.
pub fn rebuild_indexes(store: &super::Store<'_>) -> super::Result<()> {
    super::engine::memories::rebuild(&mut store.core().lock())
}
