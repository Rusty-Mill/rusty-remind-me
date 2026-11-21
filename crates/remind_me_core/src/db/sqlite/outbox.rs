//! The outbox and the sync bookkeeping on SQLite: the [`OutboxStore`] and
//! [`SyncStateStore`] adapters. Entries are queued by the other adapters'
//! writes ([`super::memories::queue`]); `sync_sends` records which remote
//! took which entry.

use super::memories::{queue, rows_where};
use super::stats::limit;
use super::{exec, SqliteTables};
use crate::db::outbox::{OutboxEntry, OutboxStore};
use crate::db::sync_state::{RemoteLog, SyncLogRow, SyncStateStore};
use crate::db::Result;
use rusqlite::{params, Row};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_sends (
    remote_id TEXT NOT NULL, outbox_id INTEGER NOT NULL, sent_at TEXT NOT NULL,
    PRIMARY KEY (remote_id, outbox_id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sync_log (
    remote_id TEXT PRIMARY KEY, last_pull TEXT NOT NULL, last_push TEXT NOT NULL,
    last_pull_id TEXT NOT NULL, last_attempt_at TEXT NOT NULL, last_push_at TEXT NOT NULL,
    last_pull_at TEXT NOT NULL, last_pull_seq INTEGER NOT NULL);
";

/// Entries no send to `?1` records.
const UNSENT_TO: &str =
    "NOT EXISTS (SELECT 1 FROM sync_sends s WHERE s.remote_id = ?1 AND s.outbox_id = o.id)";

impl OutboxStore for SqliteTables {
    fn unsent_to(&self, remote_id: &str, after_id: i64, n: usize) -> Result<Vec<OutboxEntry>> {
        self.all(
            &format!(
                "SELECT id, memory_id, payload FROM sync_outbox o WHERE id > ?2 AND sent_at = '' \
                 AND {UNSENT_TO} ORDER BY id LIMIT ?3"
            ),
            params![remote_id, after_id, limit(n)],
            |r| {
                Ok(OutboxEntry {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    payload_json: r.get(2)?,
                })
            },
        )
    }

    fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)> {
        Ok(self
            .one(
                &format!("SELECT count(*), min(created_at) FROM sync_outbox o WHERE {UNSENT_TO}"),
                [remote_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .unwrap_or((0, None)))
    }

    fn len(&self) -> Result<i64> {
        Ok(self
            .one("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))?
            .unwrap_or(0))
    }

    fn unsent_count(&self) -> Result<i64> {
        Ok(self
            .one(
                "SELECT count(*) FROM sync_outbox WHERE sent_at = ''",
                [],
                |r| r.get(0),
            )?
            .unwrap_or(0))
    }

    fn prune(&mut self, cutoff: &str, done_by: &str) -> Result<usize> {
        self.atomic(|c| {
            let gone = exec(
                c,
                "DELETE FROM sync_outbox WHERE sent_at != '' OR created_at < ?1 OR id IN \
                 (SELECT outbox_id FROM sync_sends WHERE remote_id = ?2)",
                [cutoff, done_by],
            )?;
            exec(
                c,
                "DELETE FROM sync_sends WHERE outbox_id NOT IN (SELECT id FROM sync_outbox)",
                [],
            )?;
            Ok(gone)
        })
    }

    fn clear(&mut self) -> Result<()> {
        self.atomic(|c| {
            exec(c, "DELETE FROM sync_outbox", [])?;
            exec(c, "DELETE FROM sync_sends", []).map(drop)
        })
    }

    fn backfill_everything(&mut self) -> Result<()> {
        let others = super::entities::backfill_entries(self)?;
        self.atomic(|c| {
            for row in rows_where(c, "1", "created_at, id", &[])? {
                queue(c, &row.id, "insert", &row.backfill_payload().to_string())?;
            }
            for (key, payload) in &others {
                queue(c, key, "insert", payload)?;
            }
            Ok(())
        })
    }
}

const LOG: &str = "remote_id, last_pull, last_push, last_pull_id, last_attempt_at, last_push_at, \
                   last_pull_at, last_pull_seq";

fn log_row(r: &Row<'_>) -> rusqlite::Result<SyncLogRow> {
    Ok(SyncLogRow {
        remote_id: r.get(0)?,
        last_pull: r.get(1)?,
        last_push: r.get(2)?,
        last_pull_id: r.get(3)?,
        last_attempt_at: r.get(4)?,
        last_push_at: r.get(5)?,
        last_pull_at: r.get(6)?,
        last_pull_seq: r.get(7)?,
    })
}

impl SqliteTables {
    /// Apply `change` to `remote_id`'s row, from the defaults if it has none.
    fn upsert_log(&mut self, remote_id: &str, change: impl FnOnce(&mut SyncLogRow)) -> Result<()> {
        let mut row = self
            .remote_row(remote_id)?
            .unwrap_or_else(|| SyncLogRow::new(remote_id));
        change(&mut row);
        self.put_remote_row(&row)
    }
}

impl SyncStateStore for SqliteTables {
    fn remote_row(&self, remote_id: &str) -> Result<Option<SyncLogRow>> {
        self.one(
            &format!("SELECT {LOG} FROM sync_log WHERE remote_id = ?1"),
            [remote_id],
            log_row,
        )
    }

    fn put_remote_row(&mut self, r: &SyncLogRow) -> Result<()> {
        self.exec(
            &format!(
                "INSERT OR REPLACE INTO sync_log ({LOG}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                r.remote_id,
                r.last_pull,
                r.last_push,
                r.last_pull_id,
                r.last_attempt_at,
                r.last_push_at,
                r.last_pull_at,
                r.last_pull_seq
            ],
        )
        .map(drop)
    }

    fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .remote_row(remote_id)?
            .map(|r| (r.last_pull, r.last_pull_id)))
    }

    fn set_pull_cursor(&mut self, remote_id: &str, since: &str, since_id: &str) -> Result<()> {
        self.upsert_log(remote_id, |r| {
            r.last_pull = since.to_string();
            r.last_pull_id = since_id.to_string();
        })
    }

    fn seq_cursor(&self, remote_id: &str) -> Result<Option<i64>> {
        Ok(self.remote_row(remote_id)?.map(|r| r.last_pull_seq))
    }

    fn set_seq_cursor(&mut self, remote_id: &str, seq: i64) -> Result<()> {
        self.upsert_log(remote_id, |r| r.last_pull_seq = seq)
    }

    fn reset_pull_cursors(&mut self, remote_id: &str, since: &str, seq: i64) -> Result<bool> {
        Ok(self.exec(
            "UPDATE sync_log SET last_pull = ?2, last_pull_id = '', last_pull_seq = ?3 WHERE remote_id = ?1",
            params![remote_id, since, seq],
        )? > 0)
    }

    fn stamp_push(&mut self, remote_id: &str, at: &str) -> Result<()> {
        self.upsert_log(remote_id, |r| {
            r.last_attempt_at = at.to_string();
            r.last_push_at = at.to_string();
        })
    }

    fn stamp_pull(&mut self, remote_id: &str, at: &str) -> Result<()> {
        self.upsert_log(remote_id, |r| {
            r.last_attempt_at = at.to_string();
            r.last_pull_at = at.to_string();
        })
    }

    fn last_pull_at(&self, remote_id: &str) -> Result<Option<String>> {
        Ok(self.remote_row(remote_id)?.map(|r| r.last_pull_at))
    }

    fn remotes(&self) -> Result<Vec<RemoteLog>> {
        self.all(
            "SELECT remote_id, last_attempt_at, last_push_at, last_pull_at FROM sync_log ORDER BY remote_id",
            [],
            |r| {
                Ok(RemoteLog {
                    remote_id: r.get(0)?,
                    last_attempt_at: r.get(1)?,
                    last_push_at: r.get(2)?,
                    last_pull_at: r.get(3)?,
                })
            },
        )
    }

    fn flag(&self, key: &str) -> Result<Option<String>> {
        self.one("SELECT value FROM flags WHERE key = ?1", [key], |r| {
            r.get(0)
        })
    }

    fn set_flag(&mut self, key: &str, value: &str) -> Result<()> {
        self.exec("INSERT OR REPLACE INTO flags VALUES (?1, ?2)", [key, value])
            .map(drop)
    }

    fn record_sends(&mut self, remote_id: &str, outbox_ids: &[i64], at: &str) -> Result<()> {
        self.atomic(|c| {
            for id in outbox_ids {
                exec(
                    c,
                    "INSERT OR REPLACE INTO sync_sends VALUES (?1, ?2, ?3)",
                    params![remote_id, id, at],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod conformance_tests {
    mod outbox {
        use crate::db::sqlite::SqliteTables;
        crate::db::outbox::outbox_conformance!(SqliteTables::open_in_memory().unwrap());
    }
    mod sync_state {
        use crate::db::sqlite::SqliteTables;
        crate::db::sync_state::sync_state_conformance!(SqliteTables::open_in_memory().unwrap());
    }
}
