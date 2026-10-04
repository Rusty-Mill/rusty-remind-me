//! The session commands as store operations: what the CLI's `session`,
//! `capture-transcript` and `context` subcommands run, in the process that
//! owns the store (the daemon, or the CLI itself).
//!
//! Each returns the JSON the subcommand prints, so the shape a hook parses
//! is defined once, here, and is the same whether a daemon ran it or not.
//! Paths must be absolute: the daemon's working directory is not the
//! client's.

use crate::context_brief::{self, ContextRequest};
use crate::db::{Result, Store, StoreError};
use crate::episodes;
use crate::transcript::{self, TranscriptRequest};
use serde_json::{json, Value};
use std::path::Path;

fn absolute(path: &Path, what: &str) -> Result<()> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(StoreError::Invalid(format!(
        "{what} must be an absolute path"
    )))
}

/// `{"session_id", "started_at", "start_sha"}` after recording the start.
pub fn session_start(
    store: &Store<'_>,
    session_id: &str,
    cwd: &Path,
    client: Option<&str>,
) -> Result<Value> {
    absolute(cwd, "cwd")?;
    let session = episodes::start_session(store, session_id, cwd, client)?;
    Ok(json!({
        "session_id": session.session_id,
        "started_at": session.started_at,
        "start_sha": session.start_sha,
    }))
}

/// `{"session_id", "ended_at", "end_sha", "work_log"}`; `work_log` is the
/// memory id, or `null` when the repository did not change.
pub fn session_end(
    store: &Store<'_>,
    session_id: &str,
    cwd: &Path,
    reason: Option<&str>,
) -> Result<Value> {
    absolute(cwd, "cwd")?;
    let ended = episodes::end_session(store, session_id, cwd, reason)?;
    Ok(json!({
        "session_id": ended.session.session_id,
        "ended_at": ended.session.ended_at,
        "end_sha": ended.session.end_sha,
        "work_log": ended.work_log_id,
    }))
}

/// `{"capture_id", "dialog_id", "summary_id", "messages"}` after writing or
/// refreshing the session's capture from the transcript file at `path`.
pub fn capture_transcript(
    store: &Store<'_>,
    path: &Path,
    session_id: &str,
    cwd: Option<&Path>,
    reason: Option<&str>,
) -> Result<Value> {
    absolute(path, "transcript path")?;
    if let Some(cwd) = cwd {
        absolute(cwd, "cwd")?;
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| StoreError::Invalid(format!("cannot read {}: {e}", path.display())))?;
    let outcome = transcript::capture_transcript(
        store,
        &raw,
        &TranscriptRequest {
            session_id,
            cwd,
            reason,
        },
    )?;
    serde_json::to_value(outcome).map_err(|e| StoreError::Invalid(e.to_string()))
}

/// What the `context` subcommand was asked for.
#[derive(Debug, Clone, Default)]
pub struct ContextArgs<'a> {
    pub cwd: Option<&'a Path>,
    pub project: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub budget: usize,
}

/// `{"context", "sections", "dropped"}`. The project and branch default to
/// the ones `cwd` is in.
pub fn context(store: &Store<'_>, args: &ContextArgs<'_>) -> Result<Value> {
    if let Some(cwd) = args.cwd {
        absolute(cwd, "cwd")?;
    }
    let from_cwd = args.cwd.map(|cwd| episodes::write_context(cwd, None));
    let project = args
        .project
        .map(str::to_string)
        .or_else(|| from_cwd.as_ref().and_then(|c| c.project.clone()));
    let branch = args
        .branch
        .map(str::to_string)
        .or_else(|| from_cwd.as_ref().and_then(|c| c.git_branch.clone()));
    let brief = context_brief::build(
        store,
        &ContextRequest {
            project: project.as_deref(),
            branch: branch.as_deref(),
            prompt: args.prompt,
            budget: args.budget,
        },
    )?;
    serde_json::to_value(brief).map_err(|e| StoreError::Invalid(e.to_string()))
}

/// `{"session": {...}, "memories": [...], "markdown": "..."}`, or `null` for
/// a session the store has not seen.
pub fn session_timeline(store: &Store<'_>, session_id: &str) -> Result<Value> {
    let Some((session, memories)) = episodes::timeline(store, session_id)? else {
        return Ok(Value::Null);
    };
    Ok(json!({
        "markdown": episodes::render_timeline(&session, &memories),
        "session": session,
        "memories": memories,
    }))
}

/// Sessions listed when `remind_me_session_timeline` gets no limit.
pub const DEFAULT_TIMELINE_LIMIT: usize = 10;

/// The text `remind_me_session_timeline` returns. With a `session_id`: that
/// session and its memories. Without: the recent sessions (of `project`
/// when given) with how many memories each wrote. Markdown or JSON.
pub fn timeline_text(
    store: &Store<'_>,
    session_id: Option<&str>,
    project: Option<&str>,
    limit: usize,
    markdown: bool,
) -> Result<String> {
    let json_text = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
        let Some((session, memories)) = episodes::timeline(store, id)? else {
            return Ok(format!("No session found with id {id:?}."));
        };
        return Ok(if markdown {
            episodes::render_timeline(&session, &memories)
        } else {
            json_text(&json!({ "session": session, "memories": memories }))
        });
    }
    let sessions = episodes::recent_sessions(store, limit.max(1), project)?;
    if !markdown {
        return Ok(json_text(
            &json!({ "count": sessions.len(), "sessions": sessions }),
        ));
    }
    if sessions.is_empty() {
        return Ok("_No sessions recorded._".to_string());
    }
    let lines: Vec<String> = sessions
        .iter()
        .map(|s| {
            format!(
                "- {} ({}) {} [{} memories]{}",
                s.session.session_id,
                s.session.client,
                s.session.started_at,
                s.memory_count,
                s.session
                    .project
                    .as_deref()
                    .map(|p| format!(" project: {p}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    Ok(format!("## Recent sessions\n\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn relative_paths_are_refused_and_the_shapes_are_stable() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        assert!(session_start(&store, "s1", Path::new("rel"), None).is_err());
        assert!(capture_transcript(&store, Path::new("t.jsonl"), "s1", None, None).is_err());

        let dir = std::env::temp_dir().join(format!("rrm_ops_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = session_start(&store, "s1", &dir, Some("claude-code")).unwrap();
        assert_eq!(started["session_id"], "s1");

        let ended = session_end(&store, "s1", &dir, Some("end")).unwrap();
        assert!(
            ended["work_log"].is_null(),
            "not a repository: nothing to log"
        );
        assert!(ended["ended_at"].is_string());

        let line = json!({"type": "user", "sessionId": "s1",
            "message": {"role": "user", "content": "hello there"}});
        let file = dir.join("t.jsonl");
        std::fs::write(&file, line.to_string()).unwrap();
        let captured = capture_transcript(&store, &file, "s1", Some(&dir), Some("stop")).unwrap();
        for key in ["capture_id", "dialog_id", "summary_id", "messages"] {
            assert!(captured.get(key).is_some(), "{key}");
        }
        assert!(capture_transcript(&store, &dir.join("missing"), "s1", None, None).is_err());

        let timeline = session_timeline(&store, "s1").unwrap();
        assert_eq!(timeline["memories"].as_array().unwrap().len(), 2);
        assert!(session_timeline(&store, "nope").unwrap().is_null());

        let list = timeline_text(&store, None, None, 10, true).unwrap();
        assert!(
            list.contains("s1") && list.contains("[2 memories]"),
            "{list}"
        );
        let json_list = timeline_text(&store, None, Some("nobody"), 10, false).unwrap();
        assert!(json_list.contains("\"count\": 0"));
        let missing = timeline_text(&store, Some("nope"), None, 10, true).unwrap();
        assert!(missing.contains("No session"));
        let one = timeline_text(&store, Some("s1"), None, 10, false).unwrap();
        assert!(one.contains("\"memories\""));

        let brief = context(
            &store,
            &ContextArgs {
                budget: 2000,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(brief["sections"]["recent"].as_u64().unwrap() >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
