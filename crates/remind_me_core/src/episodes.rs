//! Sessions as episodes: start, end, and what a session wrote.
//!
//! The `sessions` table (`db::sessions`) holds a session's bounds; this
//! module is the behaviour on top: stamping a write with where it happened
//! ([`write_context`]), recording a start and an end, deriving the work log
//! at the end ([`crate::worklog`]), and reading a session back as a timeline.
//!
//! The engine has no index on `memories.session_id`, and adding one would
//! change a record layout (SPEC: engine record layout rule). A session's
//! memories are therefore found by one pass over the live memories
//! ([`session_memories`]); fine at a hook's cadence, and the place to add an
//! index if sessions ever need to be read hot.

use crate::db::memories::{Memories, MemoryEdit, NewMemory};
use crate::db::sessions::{SessionRecord, SessionStart, Sessions};
use crate::db::{Result, Store, StoreError};
use crate::models::{Memory, WriteContext};
use crate::worklog::{self, WorkLog};
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;

/// Category and `memory_type` of the memory a session's end writes.
pub const WORK_LOG_CATEGORY: &str = "work_log";

/// Short project name for `cwd`: the git top-level directory's basename,
/// else `cwd`'s own.
pub fn project_of_cwd(cwd: &Path) -> Option<String> {
    let top = worklog::git(cwd, &["rev-parse", "--show-toplevel"]);
    let dir = top.as_deref().map(Path::new).unwrap_or(cwd);
    dir.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Everything a write made from `cwd` should be stamped with. Git fields are
/// `None` outside a repository. `git_remote` is left to the caller that
/// normalises remotes.
pub fn write_context(cwd: &Path, session_id: Option<&str>) -> WriteContext {
    let branch = worklog::git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    WriteContext {
        project: project_of_cwd(cwd),
        session_id: session_id.map(str::to_string),
        git_remote: None,
        git_branch: branch,
        git_sha: worklog::git(cwd, &["rev-parse", "HEAD"]),
        cwd: Some(cwd.display().to_string()),
    }
}

/// A minimal `sessions` row for `context.session_id`, if it names a session
/// the store has no row for: so a session written to outside the hook still
/// shows up. A no-op without a session id or when the row exists.
pub fn ensure_session(store: &Store<'_>, context: &WriteContext) -> Result<()> {
    let Some(session_id) = context.session_id.as_deref() else {
        return Ok(());
    };
    let sessions = Sessions::new(store);
    if sessions.get(session_id)?.is_some() {
        return Ok(());
    }
    // Not a real start, so no `start_sha`: `session start` fills it in.
    let unstarted = WriteContext {
        git_sha: None,
        ..context.clone()
    };
    sessions.upsert_start(&session_start(
        session_id,
        &unstarted,
        None,
        &Utc::now().to_rfc3339(),
    ))
}

fn session_start(
    session_id: &str,
    context: &WriteContext,
    client: Option<&str>,
    started_at: &str,
) -> SessionStart {
    let (node_id, default_client) = crate::sync::memory_provenance();
    SessionStart {
        client: client.map(str::to_string).unwrap_or(default_client),
        project: context.project.clone(),
        cwd: context.cwd.clone(),
        git_branch: context.git_branch.clone(),
        start_sha: context.git_sha.clone(),
        node_id: Some(node_id).filter(|n| !n.is_empty()),
        ..SessionStart::new(session_id, started_at)
    }
}

/// Record that `session_id` started in `cwd`. Idempotent: a second start
/// keeps the first `started_at` and `start_sha`.
pub fn start_session(
    store: &Store<'_>,
    session_id: &str,
    cwd: &Path,
    client: Option<&str>,
) -> Result<SessionRecord> {
    let id = nonempty(session_id)?;
    let context = write_context(cwd, Some(id));
    let start = session_start(id, &context, client, &Utc::now().to_rfc3339());
    let sessions = Sessions::new(store);
    sessions.upsert_start(&start)?;
    sessions.get(id)?.ok_or(StoreError::NotFound)
}

fn nonempty(session_id: &str) -> Result<&str> {
    let id = session_id.trim();
    if id.is_empty() {
        return Err(StoreError::Invalid("session id must not be empty".into()));
    }
    Ok(id)
}

/// What ending a session produced.
#[derive(Debug, Clone)]
pub struct EndOutcome {
    pub session: SessionRecord,
    /// The work-log memory, `None` when the repository did not change.
    pub work_log_id: Option<String>,
    pub work_log: Option<WorkLog>,
}

/// Record the end of `session_id`: `ended_at`, `end_sha`, and a `work_log`
/// memory derived from the repository's diff since `start_sha`. A session
/// the store never saw start is created first, without a `start_sha`. Ending
/// twice updates the same work-log memory rather than adding another.
pub fn end_session(
    store: &Store<'_>,
    session_id: &str,
    cwd: &Path,
    reason: Option<&str>,
) -> Result<EndOutcome> {
    let id = nonempty(session_id)?;
    let context = write_context(cwd, Some(id));
    ensure_session(store, &context)?;
    let sessions = Sessions::new(store);
    let before = sessions.get(id)?.ok_or(StoreError::NotFound)?;

    let log = worklog::build(cwd, before.start_sha.as_deref());
    let work_log_id = match &log {
        Some(log) => Some(write_work_log(store, &before, &context, log, reason)?),
        None => None,
    };
    sessions.end(
        id,
        &Utc::now().to_rfc3339(),
        context.git_sha.as_deref(),
        work_log_id.as_deref(),
    )?;
    Ok(EndOutcome {
        session: sessions.get(id)?.ok_or(StoreError::NotFound)?,
        work_log_id,
        work_log: log,
    })
}

/// Insert the work-log memory, or refresh the one a previous end wrote.
fn write_work_log(
    store: &Store<'_>,
    session: &SessionRecord,
    context: &WriteContext,
    log: &WorkLog,
    reason: Option<&str>,
) -> Result<String> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let content = worklog::render_markdown(log);
    let metadata = serde_json::json!({
        "work_log": log,
        "start_sha": session.start_sha,
        "end_reason": reason,
    });
    let memories = Memories::new(store);
    if let Some(existing) = session.work_log_memory_id.as_deref() {
        if memories.get_live(existing)?.is_some() {
            memories.apply_edit(
                existing,
                &MemoryEdit {
                    content: Some(content),
                    metadata: Some(metadata),
                    ..MemoryEdit::at(now_iso)
                },
            )?;
            return Ok(existing.to_string());
        }
    }

    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let decay_rate = crate::vitality::get_decay_rate(WORK_LOG_CATEGORY);
    let base_weight = crate::vitality::get_type_prior(WORK_LOG_CATEGORY);
    let (node_id, client) = crate::sync::memory_provenance();
    let mut tags = vec![WORK_LOG_CATEGORY.to_string()];
    tags.extend(context.project.clone());
    let mut row = NewMemory {
        category: WORK_LOG_CATEGORY.to_string(),
        memory_type: WORK_LOG_CATEGORY.to_string(),
        tags,
        source: "session_end".to_string(),
        metadata,
        decay_rate,
        vitality: crate::vitality::calculate_vitality(base_weight, 0, decay_rate, &now_iso, now),
        base_weight,
        accessed_at: Some(now_iso.clone()),
        node_id: Some(node_id),
        client,
        written_by: "hook".to_string(),
        capture_method: "auto".to_string(),
        ..NewMemory::new(id.as_str(), content, &now_iso)
    };
    context.apply(&mut row);
    memories.insert(&row)?;
    Ok(id)
}

/// The live memories written under `session_id`, oldest first.
pub fn session_memories(store: &Store<'_>, session_id: &str) -> Result<Vec<Memory>> {
    let mut found: Vec<Memory> = Memories::new(store)
        .all_live()?
        .into_iter()
        .filter(|m| m.session_id.as_deref() == Some(session_id))
        .collect();
    found.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(found)
}

/// A session with how many live memories carry its id.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionOverview {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub memory_count: usize,
}

/// The most recently started sessions (of `project` when given), each with
/// its memory count, from one pass over the memories.
pub fn recent_sessions(
    store: &Store<'_>,
    limit: usize,
    project: Option<&str>,
) -> Result<Vec<SessionOverview>> {
    let sessions = Sessions::new(store).recent(limit, project)?;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for memory in Memories::new(store).all_live()? {
        if let Some(id) = memory.session_id {
            *counts.entry(id).or_default() += 1;
        }
    }
    Ok(sessions
        .into_iter()
        .map(|session| SessionOverview {
            memory_count: counts.get(&session.session_id).copied().unwrap_or(0),
            session,
        })
        .collect())
}

/// One session and the memories it wrote, or `None` for an unknown id.
pub fn timeline(
    store: &Store<'_>,
    session_id: &str,
) -> Result<Option<(SessionRecord, Vec<Memory>)>> {
    let Some(session) = Sessions::new(store).get(session_id)? else {
        return Ok(None);
    };
    let memories = session_memories(store, session_id)?;
    Ok(Some((session, memories)))
}

/// Markdown for a timeline: the session's bounds, then one line per memory.
pub fn render_timeline(session: &SessionRecord, memories: &[Memory]) -> String {
    let mut out = format!("## Session {}\n\n", session.session_id);
    let field = |label: &str, value: &Option<String>| {
        value
            .as_deref()
            .map(|v| format!("- {label}: {v}\n"))
            .unwrap_or_default()
    };
    out.push_str(&format!("- client: {}\n", session.client));
    out.push_str(&field("project", &session.project));
    out.push_str(&field("branch", &session.git_branch));
    out.push_str(&format!("- started: {}\n", session.started_at));
    out.push_str(&field("ended", &session.ended_at));
    out.push_str(&field("start sha", &session.start_sha));
    out.push_str(&field("end sha", &session.end_sha));
    out.push_str(&format!("\n{} memories\n", memories.len()));
    for m in memories {
        let first = m.content.lines().next().unwrap_or("");
        let head: String = first.chars().take(100).collect();
        out.push_str(&format!("\n- {} [{}] {} ({})", m.created_at, m.category, head, m.id));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::path::PathBuf;

    fn temp_repo() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("rrm_episode_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).ok()?;
        worklog::git(&dir, &["init", "-q"])?;
        std::fs::write(dir.join("a.txt"), "one\n").ok()?;
        worklog::git(&dir, &["add", "-A"])?;
        worklog::git(
            &dir,
            &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"],
        )?;
        Some(dir)
    }

    #[test]
    fn start_is_idempotent_and_records_the_sha() {
        let Some(dir) = temp_repo() else { return };
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let first = start_session(&store, "s1", &dir, Some("claude-code")).unwrap();
        let again = start_session(&store, "s1", &dir, Some("claude-code")).unwrap();
        assert_eq!(first.started_at, again.started_at);
        assert_eq!(first.start_sha.as_deref(), worklog::git(&dir, &["rev-parse", "HEAD"]).as_deref());
        assert_eq!(first.client, "claude-code");
        assert!(first.project.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_outside_a_repository_has_no_sha() {
        let dir = std::env::temp_dir().join(format!("rrm_norepo_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open_in_memory().unwrap();
        let record = start_session(&db.store(), "s1", &dir, None).unwrap();
        assert_eq!(record.start_sha, None);
        assert!(start_session(&db.store(), "  ", &dir, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn end_writes_one_work_log_and_links_it() {
        let Some(dir) = temp_repo() else { return };
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        start_session(&store, "s1", &dir, None).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();

        let ended = end_session(&store, "s1", &dir, Some("end")).unwrap();
        let id = ended.work_log_id.clone().unwrap();
        assert_eq!(ended.session.work_log_memory_id.as_deref(), Some(id.as_str()));
        assert!(ended.session.ended_at.is_some());

        let memories = session_memories(&store, "s1").unwrap();
        assert_eq!(memories.len(), 1);
        let log = &memories[0];
        assert_eq!(log.category, "work_log");
        assert_eq!(log.memory_type.as_deref(), Some("work_log"));
        assert_eq!(log.written_by, "hook");
        assert_eq!(log.capture_method, "auto");
        assert_eq!(log.metadata["work_log"]["insertions"], 1);
        assert!(log.content.contains("a.txt"));

        // Ending again refreshes the same memory.
        let again = end_session(&store, "s1", &dir, None).unwrap();
        assert_eq!(again.work_log_id.as_deref(), Some(id.as_str()));
        assert_eq!(session_memories(&store, "s1").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn end_with_no_change_writes_no_memory_and_creates_a_missing_session() {
        let Some(dir) = temp_repo() else { return };
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let ended = end_session(&store, "never-started", &dir, None).unwrap();
        assert!(ended.work_log_id.is_none() && ended.work_log.is_none());
        assert!(ended.session.ended_at.is_some());
        assert!(session_memories(&store, "never-started").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stamped_write_creates_its_session_and_the_overview_counts_it() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let context = WriteContext {
            session_id: Some("s9".into()),
            project: Some("quokka".into()),
            ..WriteContext::default()
        };
        ensure_session(&store, &context).unwrap();
        ensure_session(&store, &WriteContext::default()).unwrap();
        let mut row = NewMemory::new("mem_1", "hello", &Utc::now().to_rfc3339());
        context.apply(&mut row);
        Memories::new(&store).insert(&row).unwrap();

        let overview = recent_sessions(&store, 10, Some("quokka")).unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].memory_count, 1);
        assert!(recent_sessions(&store, 10, Some("other")).unwrap().is_empty());

        let (session, memories) = timeline(&store, "s9").unwrap().unwrap();
        assert_eq!(memories.len(), 1);
        assert!(render_timeline(&session, &memories).contains("mem_1"));
        assert!(timeline(&store, "missing").unwrap().is_none());
    }
}
