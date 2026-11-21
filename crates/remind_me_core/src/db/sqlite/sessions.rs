//! Session rows on SQLite: the [`SessionStore`] adapter.

use super::stats::limit;
use super::SqliteTables;
use crate::db::sessions::{SessionRecord, SessionStart, SessionStore};
use crate::db::Result;
use rusqlite::{params, Row};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY, client TEXT NOT NULL, project TEXT, cwd TEXT,
    git_branch TEXT, started_at TEXT NOT NULL, ended_at TEXT, start_sha TEXT,
    end_sha TEXT, summary_memory_id TEXT, work_log_memory_id TEXT, node_id TEXT);
CREATE INDEX IF NOT EXISTS sessions_started ON sessions (started_at, session_id);
";

const COLUMNS: &str = "session_id, client, project, cwd, git_branch, started_at, ended_at, \
                       start_sha, end_sha, summary_memory_id, work_log_memory_id, node_id";

fn session(r: &Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        session_id: r.get(0)?,
        client: r.get(1)?,
        project: r.get(2)?,
        cwd: r.get(3)?,
        git_branch: r.get(4)?,
        started_at: r.get(5)?,
        ended_at: r.get(6)?,
        start_sha: r.get(7)?,
        end_sha: r.get(8)?,
        summary_memory_id: r.get(9)?,
        work_log_memory_id: r.get(10)?,
        node_id: r.get(11)?,
    })
}

impl SqliteTables {
    fn put_session(&self, s: &SessionRecord) -> Result<()> {
        self.exec(
            &format!("INSERT OR REPLACE INTO sessions ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"),
            params![
                s.session_id,
                s.client,
                s.project,
                s.cwd,
                s.git_branch,
                s.started_at,
                s.ended_at,
                s.start_sha,
                s.end_sha,
                s.summary_memory_id,
                s.work_log_memory_id,
                s.node_id
            ],
        )
        .map(drop)
    }
}

impl SessionStore for SqliteTables {
    fn upsert_start(&mut self, start: &SessionStart) -> Result<()> {
        let current = self.get(&start.session_id)?;
        self.put_session(&SessionRecord::started(current, start))
    }

    fn end(
        &mut self,
        id: &str,
        ended_at: &str,
        end_sha: Option<&str>,
        work_log: Option<&str>,
    ) -> Result<bool> {
        let Some(mut current) = self.get(id)? else {
            return Ok(false);
        };
        current.ended(ended_at, end_sha, work_log);
        self.put_session(&current)?;
        Ok(true)
    }

    fn get(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        self.one(
            &format!("SELECT {COLUMNS} FROM sessions WHERE session_id = ?1"),
            [session_id],
            session,
        )
    }

    fn recent(&self, n: usize, project: Option<&str>) -> Result<Vec<SessionRecord>> {
        self.all(
            &format!(
                "SELECT {COLUMNS} FROM sessions WHERE ?1 IS NULL OR project = ?1 \
                 ORDER BY started_at DESC, session_id DESC LIMIT ?2"
            ),
            params![project, limit(n)],
            session,
        )
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::sessions::session_conformance!(SqliteTables::open_in_memory().unwrap());
}
