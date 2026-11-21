//! Dialog rows are the verbatim record: no generic path edits or deletes one.
//!
//! A dialog (category [`DIALOG_CATEGORY`]) is often the only copy of a
//! conversation, and a delete cannot be undone (ADR-0024). Until the
//! append-only transcript store lands (#528), the record is protected where
//! it sits: every generic write path that would edit, supersede, merge or
//! delete a live dialog asks [`refuse_dialog_edit`] first, the same way
//! project claims are protected by [`crate::project::refuse_claim_edit`].
//!
//! Writes that do not go through the generic paths stay: a capture writes
//! its dialog, and a session transcript's recapture grows it in place with a
//! `recapture` revision of what it replaced (`transcript.rs`). Reads, and the
//! access bookkeeping a read writes, are not edits.

use crate::db::{Result, StoreError};
use crate::models::{Memory, DIALOG_CATEGORY};

/// Whether `memory` is a protected dialog row.
pub fn is_dialog(memory: &Memory) -> bool {
    memory.category == DIALOG_CATEGORY
}

/// Refuse an edit, supersede, merge or delete of a dialog row.
pub fn refuse_dialog_edit(memory: &Memory) -> Result<()> {
    if !is_dialog(memory) {
        return Ok(());
    }
    Err(StoreError::Invalid(format!(
        "memory {} is a dialog (the verbatim record) and cannot be edited or deleted",
        memory.id
    )))
}

/// Refuse an edit or delete of a memory that is append-only: a project claim
/// or a dialog row.
pub fn refuse_protected_edit(memory: &Memory) -> Result<()> {
    crate::project::refuse_claim_edit(memory)?;
    refuse_dialog_edit(memory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(category: &str) -> Memory {
        let mut row = crate::db::memories::NewMemory::new("m1", "said", "2026-11-21T00:00:00Z");
        row.category = category.into();
        crate::db::engine::memories::MemoryRow::from_new(&row).to_memory()
    }

    #[test]
    fn a_dialog_is_refused_with_its_id() {
        let refused = refuse_dialog_edit(&memory(DIALOG_CATEGORY)).unwrap_err();
        assert!(refused.to_string().contains("m1 is a dialog"), "{refused}");
    }

    #[test]
    fn other_categories_pass() {
        for category in ["conversation", "skeleton", "fact", "general"] {
            assert!(refuse_dialog_edit(&memory(category)).is_ok(), "{category}");
        }
    }
}
