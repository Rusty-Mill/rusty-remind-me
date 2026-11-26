//! Revision history on SQLite: the [`HistoryStore`] adapter.

use super::SqliteTables;
use crate::db::derived::Origin;
use crate::db::history::{HistoryStore, Tracked};
use crate::db::memories::MemoryStore;
use crate::db::Result;
use crate::models::MemoryRevision;
use rusqlite::{params, Row};

/// `project_known` is 0 for a revision from before the project was
/// recorded (`Tracked::project` is `None`), 1 otherwise.
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT, memory_id TEXT NOT NULL,
    content TEXT NOT NULL, category TEXT NOT NULL, tags TEXT NOT NULL,
    metadata TEXT NOT NULL, sensitive INTEGER, edited_at TEXT NOT NULL,
    revision_reason TEXT, project TEXT, project_known INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS memory_revisions_memory ON memory_revisions (memory_id);
";

const COLUMNS: &str = "id, memory_id, content, category, tags, metadata, sensitive, edited_at, \
                       revision_reason, project, project_known";

fn revision(r: &Row<'_>) -> rusqlite::Result<MemoryRevision> {
    let known: bool = r.get(10)?;
    Ok(MemoryRevision {
        id: r.get(0)?,
        memory_id: r.get(1)?,
        content: r.get(2)?,
        category: r.get(3)?,
        tags: r.get(4)?,
        metadata: r.get(5)?,
        sensitive: r.get(6)?,
        edited_at: r.get(7)?,
        revision_reason: r.get(8)?,
        project: known.then(|| r.get(9)).transpose()?,
    })
}

fn tracked(r: MemoryRevision) -> Tracked {
    Tracked {
        content: r.content,
        category: r.category,
        tags: r.tags,
        metadata: r.metadata,
        sensitive: r.sensitive,
        project: r.project,
    }
}

impl HistoryStore for SqliteTables {
    fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(self.get_live(memory_id)?.is_some())
    }

    fn current(&self, memory_id: &str) -> Result<Option<Tracked>> {
        Ok(self.row(memory_id)?.map(|row| Tracked {
            content: row.content,
            category: row.category,
            tags: row.tags,
            metadata: row.metadata,
            sensitive: Some(row.sensitive),
            project: Some(row.project),
        }))
    }

    fn delete_for(&mut self, memory_id: &str) -> Result<()> {
        self.exec(
            "DELETE FROM memory_revisions WHERE memory_id = ?1",
            [memory_id],
        )
        .map(drop)
    }

    fn delete_of_tombstones(&mut self) -> Result<usize> {
        self.exec(
            "DELETE FROM memory_revisions WHERE memory_id IN \
             (SELECT id FROM memories WHERE deleted_at IS NOT NULL)",
            [],
        )
    }

    fn insert(
        &mut self,
        memory_id: &str,
        values: &Tracked,
        edited_at: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "INSERT INTO memory_revisions (memory_id, content, category, tags, metadata, sensitive, \
             edited_at, revision_reason, project, project_known) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                memory_id,
                values.content,
                values.category,
                values.tags,
                values.metadata,
                values.sensitive,
                edited_at,
                reason,
                values.project.clone().flatten(),
                values.project.is_some(),
            ],
        )
        .map(drop)
    }

    fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
        self.all(
            &format!(
                "SELECT {COLUMNS} FROM memory_revisions WHERE memory_id = ?1 \
                 ORDER BY edited_at DESC, id DESC LIMIT ?2"
            ),
            params![memory_id, i64::try_from(limit).unwrap_or(i64::MAX)],
            revision,
        )
    }

    /// Walks the memory's revisions by id, newest first, comparing each
    /// with the next newer one's content (or `?2` for the newest), and
    /// stops at the first that differs.
    fn content_revised_at(&self, memory_id: &str, content: &str) -> Result<Option<String>> {
        self.one(
            "SELECT edited_at FROM memory_revisions r WHERE r.memory_id = ?1 \
             AND r.content != COALESCE((SELECT n.content FROM memory_revisions n \
                 WHERE n.memory_id = ?1 AND n.id > r.id ORDER BY n.id LIMIT 1), ?2) \
             ORDER BY r.id DESC LIMIT 1",
            params![memory_id, content],
            |r| r.get(0),
        )
    }

    fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        Ok(self
            .one(
                &format!("SELECT {COLUMNS} FROM memory_revisions WHERE id = ?1 AND memory_id = ?2"),
                params![revision_id, memory_id],
                revision,
            )?
            .map(tracked))
    }

    fn restore(&mut self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        self.update_memory(memory_id, Origin::Local, |row| {
            // A revert never rewrites a dialog (#1616).
            crate::dialog::refuse_category(&row.id, &row.category)?;
            row.content.clone_from(&values.content);
            row.category.clone_from(&values.category);
            row.tags.clone_from(&values.tags);
            row.metadata.clone_from(&values.metadata);
            row.sensitive = values.sensitive.unwrap_or(false);
            if let Some(project) = &values.project {
                row.project.clone_from(project);
            }
            row.updated_at = updated_at.to_string();
            Ok(())
        })
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::history::history_conformance!(SqliteTables::open_in_memory().unwrap());
}
