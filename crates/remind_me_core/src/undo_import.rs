//! Rolling back a bulk import.
//!
//! Imports are the one bulk write this crate makes, and `remind_me_delete`
//! takes a single id — unusable at import scale, where a mempalace run can be
//! tens of thousands of records.
//!
//! Two things make this more than a `DELETE ... WHERE`:
//!
//! **Deletion goes through [`crate::db::queries::delete_memory`]**, the same
//! path `remind_me_delete` uses, so chunk vectors, entity mention links,
//! feedback and associations are cleaned up. A hand-written bulk `DELETE`
//! against `memories` would orphan every one of them — and orphaned
//! `vec_chunks` rows are actively dangerous, because SQLite reuses freed
//! rowids and a later memory landing on the same one would silently inherit
//! them.
//!
//! **The import's tracking rows go too.** Every import path treats a tracked
//! id as already done and skips it, so leaving those behind would make the
//! same content permanently un-importable — the undo would appear to work and
//! then silently refuse to let you redo it.
//!
//! On a sync-enabled node this is a soft delete, so the removal propagates
//! rather than resurrecting on the next pull. That also means the space is not
//! reclaimed until tombstones are compacted.

use crate::db::queries::delete_memory;
use crate::models::{UndoImportInput, UndoImportKind, UndoImportResult};
use rusqlite::{params_from_iter, Connection, Result};

/// Memory ids belonging to an import, plus a human-readable scope label.
///
/// The three kinds resolve differently because they track differently.
/// `chat_imports` has no `memory_id` column — it keys on `import_id`, which the
/// importer stamps onto `memories.doc_id`, so that join runs the other way
/// round from the other two.
fn matching_ids(
    conn: &Connection,
    kind: UndoImportKind,
    import_id: Option<&str>,
) -> Result<(Vec<String>, String)> {
    match kind {
        UndoImportKind::Chat => match import_id {
            Some(id) => {
                let mut stmt = conn
                    .prepare("SELECT id FROM memories WHERE doc_id = ? AND deleted_at IS NULL")?;
                let ids = stmt
                    .query_map([id], |r| r.get(0))?
                    .collect::<Result<Vec<String>>>()?;
                Ok((ids, format!("chat import {}", id)))
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id FROM memories
                      WHERE deleted_at IS NULL
                        AND doc_id IN (SELECT import_id FROM chat_imports)",
                )?;
                let ids = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<Vec<String>>>()?;
                Ok((ids, "all chat imports".to_string()))
            }
        },
        UndoImportKind::Mempalace => matching_ids_mempalace(conn, import_id),
        UndoImportKind::Dbs => {
            // Prefix match so a whole dbs source can be targeted without naming
            // every external id it produced.
            let (sql, label) = match import_id {
                Some(id) => (
                    "SELECT t.memory_id FROM dbs_imports t
                       JOIN memories m ON m.id = t.memory_id
                      WHERE m.deleted_at IS NULL AND t.dbs_source LIKE ?",
                    format!("dbs scope '{}'", id),
                ),
                None => (
                    "SELECT t.memory_id FROM dbs_imports t
                       JOIN memories m ON m.id = t.memory_id
                      WHERE m.deleted_at IS NULL",
                    "all dbs imports".to_string(),
                ),
            };
            // One binding rather than a `match` returning two `query_map`s:
            // no two closures share a type, even identical ones, so the arms
            // would not unify.
            let bindings: Vec<String> =
                import_id.map(|id| format!("{}%", id)).into_iter().collect();
            let mut stmt = conn.prepare(sql)?;
            let ids = stmt
                .query_map(params_from_iter(bindings.iter()), |r| r.get(0))?
                .collect::<Result<Vec<String>>>()?;
            Ok((ids, label))
        }
    }
}

/// Mempalace ids, tolerating content that never went through the tracked path.
///
/// `remind_me_import_mempalace` records a `(drawer_id -> memory_id)` row for
/// every drawer it writes, so the tracking join is the precise signal. But
/// mempalace-shaped content can reach the store without it — a bulk load
/// predating the tracking table, say — and that content is still unambiguously
/// mempalace: every write carries `source` of `mempalace_import` or
/// `mempalace:<original>`, plus `metadata.mempalace_drawer_id`.
///
/// Both signals are unioned and deduplicated, so an undo covers the batch
/// regardless of which path wrote it. Trusting the tracking table alone would
/// silently leave the untracked half behind.
fn matching_ids_mempalace(
    conn: &Connection,
    import_id: Option<&str>,
) -> Result<(Vec<String>, String)> {
    let mut ids: std::collections::BTreeSet<String> = Default::default();

    let (tracked_sql, untracked_sql, label) = match import_id {
        Some(id) => (
            "SELECT t.memory_id FROM mempalace_imports t
               JOIN memories m ON m.id = t.memory_id
              WHERE m.deleted_at IS NULL AND t.drawer_id LIKE ?",
            "SELECT id FROM memories
              WHERE deleted_at IS NULL
                AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')
                AND json_extract(metadata, '$.mempalace_drawer_id') LIKE ?",
            format!("mempalace scope '{}'", id),
        ),
        None => (
            "SELECT t.memory_id FROM mempalace_imports t
               JOIN memories m ON m.id = t.memory_id
              WHERE m.deleted_at IS NULL",
            "SELECT id FROM memories
              WHERE deleted_at IS NULL
                AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')",
            "all mempalace imports".to_string(),
        ),
    };

    let bindings: Vec<String> = import_id.map(|id| format!("{}%", id)).into_iter().collect();
    for sql in [tracked_sql, untracked_sql] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params_from_iter(bindings.iter()), |r| r.get::<_, String>(0))?;
        for row in rows {
            ids.insert(row?);
        }
    }

    Ok((ids.into_iter().collect(), label))
}

/// Drop the tracking rows for ids just removed.
///
/// `doc_ids` must have been captured *before* the purge: for chat imports the
/// link lives on `memories.doc_id`, and a hard delete removes those rows,
/// leaving nothing to read the import id back from afterwards.
fn forget_tracking(
    conn: &Connection,
    kind: UndoImportKind,
    memory_ids: &[String],
    doc_ids: &[String],
) -> Result<usize> {
    if memory_ids.is_empty() {
        return Ok(0);
    }

    match kind {
        UndoImportKind::Chat => {
            if doc_ids.is_empty() {
                return Ok(0);
            }
            // `chat_imports` rows are per-file, not per-memory. Dropping one
            // while some of its chunks survive would let a re-import duplicate
            // the surviving half, so an import only loses its tracking row once
            // nothing of it is left.
            let marks = vec!["?"; doc_ids.len()].join(",");
            const SURVIVORS: &str = "SELECT doc_id FROM memories
                             WHERE doc_id IS NOT NULL AND deleted_at IS NULL";

            // Retained raw transcripts (#212) go with the tracking row, and are
            // read out *before* the delete because the archive row is what
            // holds the blob's path. Selected with the same NOT IN clause the
            // delete uses rather than a looser one: forgetting the archive of
            // an import whose chunks survive would strand memories pointing at
            // a file that is no longer there.
            let doomed: Vec<String> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT import_id FROM chat_imports
                      WHERE import_id IN ({marks})
                        AND import_id NOT IN ({SURVIVORS})",
                    marks = marks,
                    SURVIVORS = SURVIVORS
                ))?;
                // Bound rather than returned directly: as the block's tail
                // expression the iterator's temporary outlives `stmt`.
                let rows = stmt
                    .query_map(params_from_iter(doc_ids.iter()), |r| r.get(0))?
                    .collect::<Result<Vec<String>>>()?;
                rows
            };
            for import_id in &doomed {
                crate::archive::forget_import(conn, import_id)?;
            }

            let affected = conn.execute(
                &format!(
                    "DELETE FROM chat_imports
                      WHERE import_id IN ({marks})
                        AND import_id NOT IN ({SURVIVORS})",
                    marks = marks,
                    SURVIVORS = SURVIVORS
                ),
                params_from_iter(doc_ids.iter()),
            )?;
            Ok(affected)
        }
        UndoImportKind::Dbs | UndoImportKind::Mempalace => {
            let table = match kind {
                UndoImportKind::Dbs => "dbs_imports",
                _ => "mempalace_imports",
            };
            let marks = vec!["?"; memory_ids.len()].join(",");
            let affected = conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE memory_id IN ({marks})",
                    table = table,
                    marks = marks
                ),
                params_from_iter(memory_ids.iter()),
            )?;
            Ok(affected)
        }
    }
}

/// Roll back an import, or report what rolling it back would remove.
///
/// Defaults to a dry run, deliberately: this is a bulk destructive operation
/// and, on a sync-enabled node, one that propagates to every other node. The
/// work is resumable — call again until `remaining` reaches 0.
pub fn undo_import(conn: &Connection, input: &UndoImportInput) -> Result<UndoImportResult> {
    let (memory_ids, scope) = matching_ids(conn, input.import_kind, input.import_id.as_deref())?;
    let soft = crate::sync::sync_enabled();

    let mode = if soft {
        "soft-delete (tombstone, propagates over sync)"
    } else {
        "hard delete (sync disabled — nothing to propagate to)"
    };

    if input.dry_run {
        return Ok(UndoImportResult {
            import_kind: input.import_kind,
            scope,
            matched: memory_ids.len(),
            dry_run: true,
            mode: mode.to_string(),
            removed: 0,
            remaining: memory_ids.len(),
            tracking_rows_removed: 0,
            hint: Some(
                if soft {
                    "dry run — nothing changed. Re-run with dry_run=false to remove. \
                     Tombstoned rows keep their content until compaction, so disk use \
                     will not drop immediately."
                } else {
                    "dry run — nothing changed. Re-run with dry_run=false to remove. \
                     Rows are removed outright; run VACUUM to reclaim the file."
                }
                .to_string(),
            ),
        });
    }

    let batch: Vec<String> = memory_ids.iter().take(input.limit).cloned().collect();

    // Before the purge, not after: a hard delete removes the rows outright, and
    // `doc_id` is the only place a chat import's id is recorded on the memory.
    let doc_ids: Vec<String> = if input.import_kind == UndoImportKind::Chat && !batch.is_empty() {
        let marks = vec!["?"; batch.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT doc_id FROM memories
              WHERE doc_id IS NOT NULL AND id IN ({})",
            marks
        ))?;
        let ids = stmt
            .query_map(params_from_iter(batch.iter()), |r| r.get(0))?
            .collect::<Result<Vec<String>>>()?;
        ids
    } else {
        Vec::new()
    };

    let mut removed = 0usize;
    for memory_id in &batch {
        // `delete_memory` reports false for an id already gone, which is not an
        // error here: a resumed undo can legitimately re-encounter one.
        if delete_memory(conn, memory_id)? {
            removed += 1;
        }
    }

    let tracking_rows_removed = forget_tracking(conn, input.import_kind, &batch, &doc_ids)?;

    let remaining = memory_ids.len().saturating_sub(removed);
    Ok(UndoImportResult {
        import_kind: input.import_kind,
        scope,
        matched: memory_ids.len(),
        dry_run: false,
        mode: mode.to_string(),
        removed,
        remaining,
        tracking_rows_removed,
        hint: (remaining > 0).then(|| "call again to continue — work is resumable".to_string()),
    })
}
