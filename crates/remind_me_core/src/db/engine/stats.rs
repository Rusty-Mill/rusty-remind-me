//! Store-wide counts over memories on the engine core (ADR-0023, core PR
//! 2d), as the engine's adapter for the [`StatsStore`] port (ADR-0035). The
//! chat-import count is `db::engine::imports`' (core PR 4a).

use super::memories::{self, MemoryRow};
use super::{core_ref, imports, EngineTables};
use crate::db::stats::{GroupBy, StatsStore};
use crate::db::Result;
use crate::models::DigestRecentMemory;
use crate::stats::RecentMemory;
use std::collections::{BTreeMap, HashSet};

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn live(tables: &EngineTables) -> Result<Vec<MemoryRow>> {
    Ok(memories::rows(core_ref(tables)?)
        .filter(|row| row.deleted_at.is_none())
        .collect())
}

fn live_memories(tables: &EngineTables) -> Result<i64> {
    Ok(count(live(tables)?.len()))
}

fn count_by(tables: &EngineTables, group: GroupBy) -> Result<BTreeMap<String, i64>> {
    let mut counts = BTreeMap::new();
    for row in live(tables)? {
        let key = match group {
            GroupBy::Category => row.category,
            GroupBy::Source => row.source,
            GroupBy::Project => match row.project {
                Some(project) => project,
                None => continue,
            },
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    Ok(counts)
}

/// Live memories counted by tag, through the tag index.
fn count_by_tag(tables: &EngineTables) -> Result<BTreeMap<String, i64>> {
    let live: HashSet<String> = live(tables)?.into_iter().map(|row| row.id).collect();
    let core = core_ref(tables)?;
    Ok(core
        .tags
        .entries()
        .map(|(tag, ids)| (tag.clone(), count(ids.intersection(&live).count())))
        .filter(|(_, n)| *n > 0)
        .collect())
}

/// Every stored memory, and how many of them are tombstones.
fn memory_totals(tables: &EngineTables) -> Result<(i64, i64)> {
    let rows: Vec<MemoryRow> = memories::rows(core_ref(tables)?).collect();
    let tombstones = rows.iter().filter(|row| row.deleted_at.is_some()).count();
    Ok((count(rows.len()), count(tombstones)))
}

/// Tombstones that still hold their text.
fn tombstones_holding_text(tables: &EngineTables) -> Result<i64> {
    Ok(count(
        memories::rows(core_ref(tables)?)
            .filter(|row| row.deleted_at.is_some() && !row.is_emptied())
            .count(),
    ))
}

/// Every stored memory by category, an empty category counted as `(none)`.
fn all_by_category(tables: &EngineTables) -> Result<BTreeMap<String, i64>> {
    let mut counts = BTreeMap::new();
    for row in memories::rows(core_ref(tables)?) {
        let key = if row.category.is_empty() {
            "(none)".to_string()
        } else {
            row.category
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    Ok(counts)
}

/// Live, non-sensitive memories created at or after `cutoff`, newest first.
fn shareable(tables: &EngineTables, cutoff: &str) -> Result<Vec<MemoryRow>> {
    let mut rows: Vec<MemoryRow> = live(tables)?
        .into_iter()
        .filter(|row| !row.sensitive && row.created_at.as_str() >= cutoff)
        .collect();
    newest_first(&mut rows);
    Ok(rows)
}

fn newest_first(rows: &mut [MemoryRow]) {
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
}

fn shareable_since(
    tables: &EngineTables,
    cutoff: &str,
    limit: usize,
) -> Result<Vec<DigestRecentMemory>> {
    Ok(shareable(tables, cutoff)?
        .into_iter()
        .take(limit)
        .map(|row| DigestRecentMemory {
            id: row.id,
            content: row.content,
            category: row.category,
            created_at: row.created_at,
        })
        .collect())
}

fn count_shareable_since(tables: &EngineTables, cutoff: &str) -> Result<i64> {
    Ok(count(shareable(tables, cutoff)?.len()))
}

/// The `limit` newest live memories, content cut to 80 characters as
/// SQLite's `substr` counts them.
fn recent(tables: &EngineTables, limit: usize) -> Result<Vec<RecentMemory>> {
    let mut rows = live(tables)?;
    newest_first(&mut rows);
    Ok(rows
        .into_iter()
        .take(limit)
        .map(|row| RecentMemory {
            preview: row.content.chars().take(80).collect(),
            id: row.id,
            category: row.category,
            created_at: row.created_at,
        })
        .collect())
}

impl StatsStore for EngineTables {
    fn live_memories(&self) -> Result<i64> {
        live_memories(self)
    }

    fn imports(&self) -> Result<i64> {
        imports::chat_import_count(self)
    }

    fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        count_by(self, group)
    }

    fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        count_by_tag(self)
    }

    fn memory_totals(&self) -> Result<(i64, i64)> {
        memory_totals(self)
    }

    fn tombstones_holding_text(&self) -> Result<i64> {
        tombstones_holding_text(self)
    }

    fn all_by_category(&self) -> Result<BTreeMap<String, i64>> {
        all_by_category(self)
    }

    fn shareable_since(&self, cutoff: &str, limit: usize) -> Result<Vec<DigestRecentMemory>> {
        shareable_since(self, cutoff, limit)
    }

    fn count_shareable_since(&self, cutoff: &str) -> Result<i64> {
        count_shareable_since(self, cutoff)
    }

    fn recent(&self, limit: usize) -> Result<Vec<RecentMemory>> {
        recent(self, limit)
    }

    /// Every file in the engine directory, the lock file included.
    fn size_on_disk(&self) -> Result<u64> {
        EngineTables::size_on_disk(self)
    }
}

/// The port's shared behaviour, on fresh temporary engine tables.
#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;

    crate::db::stats::stats_conformance!(EngineTables::open_temporary().unwrap());
}
