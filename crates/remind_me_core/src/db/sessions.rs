//! Storage for `sessions` (schema v32): each client session's start and
//! end, so a memory's `session_id` leads to when and where it was written
//! and what the session changed.
//!
//! Storage only. The CLI that records a session, and the work log written
//! at its end, are later steps. The merge rules sit on [`SessionRecord`],
//! beside the row they describe, so the engine repository is a thin layer.
//!
//! Added at schema v32, once the engine was the node's store (ADR-0023),
//! as [`super::references`] was: the table never lived in SQLite.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use serde::{Deserialize, Serialize};

/// What a session's start records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStart {
    pub session_id: String,
    /// `claude-code`, `claude-desktop`, … or `unknown`.
    pub client: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: String,
    /// HEAD when the session started.
    pub start_sha: Option<String>,
    pub node_id: Option<String>,
}

impl SessionStart {
    /// A start for `session_id` at `started_at`, by an unknown client,
    /// knowing nothing else.
    pub fn new(session_id: impl Into<String>, started_at: &str) -> Self {
        Self {
            session_id: session_id.into(),
            client: "unknown".to_string(),
            project: None,
            cwd: None,
            git_branch: None,
            started_at: started_at.to_string(),
            start_sha: None,
            node_id: None,
        }
    }
}

/// One `sessions` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub client: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub start_sha: Option<String>,
    pub end_sha: Option<String>,
    pub summary_memory_id: Option<String>,
    pub work_log_memory_id: Option<String>,
    pub node_id: Option<String>,
}

impl SessionRecord {
    /// The row after `start` is recorded over `current`, if any: a new row
    /// takes everything the start knows; an existing one keeps its first
    /// `started_at` and `start_sha` (a hook that fires twice must not move
    /// the session's beginning), takes the client as given, fills in any
    /// column it was missing, and keeps how it ended.
    pub fn started(current: Option<Self>, start: &SessionStart) -> Self {
        let Some(mut row) = current else {
            return Self {
                session_id: start.session_id.clone(),
                client: start.client.clone(),
                project: start.project.clone(),
                cwd: start.cwd.clone(),
                git_branch: start.git_branch.clone(),
                started_at: start.started_at.clone(),
                ended_at: None,
                start_sha: start.start_sha.clone(),
                end_sha: None,
                summary_memory_id: None,
                work_log_memory_id: None,
                node_id: start.node_id.clone(),
            };
        };
        row.client.clone_from(&start.client);
        row.project = start.project.clone().or(row.project);
        row.cwd = start.cwd.clone().or(row.cwd);
        row.git_branch = start.git_branch.clone().or(row.git_branch);
        row.start_sha = row.start_sha.or_else(|| start.start_sha.clone());
        row.node_id = start.node_id.clone().or(row.node_id);
        row
    }

    /// Record the end: `ended_at` always, the sha and work log when given.
    pub fn ended(
        &mut self,
        ended_at: &str,
        end_sha: Option<&str>,
        work_log_memory_id: Option<&str>,
    ) {
        self.ended_at = Some(ended_at.to_string());
        if end_sha.is_some() {
            self.end_sha = end_sha.map(str::to_string);
        }
        if work_log_memory_id.is_some() {
            self.work_log_memory_id = work_log_memory_id.map(str::to_string);
        }
    }
}

/// The `sessions` table on the engine's memories core
/// (`db::engine::sessions`).
pub struct Sessions<'c> {
    core: &'c EngineLock,
}

impl<'c> Sessions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Record that a session started, as [`SessionRecord::started`] says.
    /// Idempotent: a repeated start leaves the row as the first left it,
    /// apart from columns the first did not know.
    pub fn upsert_start(&self, start: &SessionStart) -> Result<()> {
        engine::sessions::upsert_start(&mut self.core.lock(), start)
    }

    /// Record that `session_id` ended, as [`SessionRecord::ended`] says.
    /// Whether there was a session to end.
    pub fn end(
        &self,
        session_id: &str,
        ended_at: &str,
        end_sha: Option<&str>,
        work_log_memory_id: Option<&str>,
    ) -> Result<bool> {
        engine::sessions::end(
            &mut self.core.lock(),
            session_id,
            ended_at,
            end_sha,
            work_log_memory_id,
        )
    }

    /// Session `session_id`, if recorded.
    pub fn get(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        engine::sessions::get(&self.core.lock(), session_id)
    }

    /// The most recently started sessions, of `project` when given, newest
    /// first (ties by id, descending), at most `limit`.
    pub fn recent(&self, limit: usize, project: Option<&str>) -> Result<Vec<SessionRecord>> {
        engine::sessions::recent(&self.core.lock(), limit, project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn on_engine(test: impl FnOnce(&Database)) {
        test(&Database::open_in_memory().unwrap());
    }

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";
    const T3: &str = "2026-09-28T00:00:00+00:00";

    fn start(id: &str, at: &str, project: Option<&str>) -> SessionStart {
        SessionStart {
            client: "claude-code".into(),
            project: project.map(str::to_string),
            cwd: Some(format!("/work/{id}")),
            git_branch: Some("main".into()),
            start_sha: Some(format!("sha-{id}")),
            node_id: Some("node".into()),
            ..SessionStart::new(id, at)
        }
    }

    #[test]
    fn a_repeated_start_keeps_the_first_beginning_and_fills_gaps() {
        on_engine(|db| {
            let store = db.store();
            let sessions = Sessions::new(&store);
            assert!(sessions.get("s1").unwrap().is_none());

            // The first start knows no project or sha.
            sessions
                .upsert_start(&SessionStart {
                    start_sha: None,
                    project: None,
                    ..start("s1", T1, None)
                })
                .unwrap();
            // A later one does, and claims a later beginning and a
            // different cwd.
            sessions
                .upsert_start(&SessionStart {
                    cwd: Some("/elsewhere".into()),
                    client: "hook".into(),
                    ..start("s1", T2, Some("quokka"))
                })
                .unwrap();

            assert_eq!(
                sessions.get("s1").unwrap().unwrap(),
                SessionRecord {
                    session_id: "s1".into(),
                    client: "hook".into(),
                    project: Some("quokka".into()),
                    cwd: Some("/elsewhere".into()),
                    git_branch: Some("main".into()),
                    started_at: T1.into(),
                    ended_at: None,
                    start_sha: Some("sha-s1".into()),
                    end_sha: None,
                    summary_memory_id: None,
                    work_log_memory_id: None,
                    node_id: Some("node".into()),
                }
            );
        });
    }

    #[test]
    fn end_sets_the_end_fields_and_reports_a_missing_session() {
        on_engine(|db| {
            let store = db.store();
            let sessions = Sessions::new(&store);
            sessions.upsert_start(&start("s1", T1, None)).unwrap();

            assert!(sessions.end("s1", T2, Some("sha-end"), None).unwrap());
            let ended = sessions.get("s1").unwrap().unwrap();
            assert_eq!(ended.ended_at.as_deref(), Some(T2));
            assert_eq!(ended.end_sha.as_deref(), Some("sha-end"));
            assert_eq!(ended.work_log_memory_id, None);
            assert_eq!(ended.started_at, T1, "the beginning is untouched");

            // Ending again with only a work log keeps the sha.
            assert!(sessions.end("s1", T3, None, Some("mem_log")).unwrap());
            let again = sessions.get("s1").unwrap().unwrap();
            assert_eq!(again.ended_at.as_deref(), Some(T3));
            assert_eq!(again.end_sha.as_deref(), Some("sha-end"));
            assert_eq!(again.work_log_memory_id.as_deref(), Some("mem_log"));

            assert!(!sessions.end("missing", T2, None, None).unwrap());
            // A start after the end keeps the end.
            sessions.upsert_start(&start("s1", T3, None)).unwrap();
            assert_eq!(
                sessions.get("s1").unwrap().unwrap().ended_at.as_deref(),
                Some(T3)
            );
        });
    }

    #[test]
    fn recent_orders_newest_first_and_filters_by_project() {
        on_engine(|db| {
            let store = db.store();
            let sessions = Sessions::new(&store);
            sessions.upsert_start(&start("a", T1, Some("p1"))).unwrap();
            sessions.upsert_start(&start("b", T3, Some("p2"))).unwrap();
            sessions.upsert_start(&start("c", T2, Some("p1"))).unwrap();
            sessions.upsert_start(&start("d", T2, None)).unwrap();

            let ids = |rows: Vec<SessionRecord>| -> Vec<String> {
                rows.into_iter().map(|r| r.session_id).collect()
            };
            assert_eq!(
                ids(sessions.recent(10, None).unwrap()),
                ["b", "d", "c", "a"]
            );
            assert_eq!(ids(sessions.recent(2, None).unwrap()), ["b", "d"]);
            assert_eq!(ids(sessions.recent(10, Some("p1")).unwrap()), ["c", "a"]);
            assert!(sessions.recent(10, Some("p9")).unwrap().is_empty());
            assert!(sessions.recent(0, None).unwrap().is_empty());
        });
    }
}
