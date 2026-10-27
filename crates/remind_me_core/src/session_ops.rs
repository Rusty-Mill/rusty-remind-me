//! The session commands as store operations: what the CLI's `session`,
//! `capture-transcript` and `context` subcommands run, in the process that
//! owns the store (the daemon, or the CLI itself).
//!
//! Each returns the JSON the subcommand prints, so the shape a hook parses
//! is defined once, here, and is the same whether a daemon ran it or not.
//! Paths must be absolute: the daemon's working directory is not the
//! client's.

use crate::context_brief::{self, Brief, ContextRequest, Mode, Outcome};
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
            context: None,
        },
    )?;
    serde_json::to_value(outcome).map_err(|e| StoreError::Invalid(e.to_string()))
}

/// `{"imported", "skipped", "empty", "failed": [{"path", "error"}]}` after a
/// one-time backfill of a Claude Code `projects` folder: every
/// `dir/*/*.jsonl` session not captured yet is captured, keyed by the
/// session id (else the file stem) the transcript carries and stamped only
/// with where the transcript says it ran ([`transcript_context`]). A session
/// already captured, by the hook or an earlier run, is left as it is, so a
/// re-run imports nothing twice. A file that cannot be read or stored is
/// reported in `failed` and the walk goes on.
pub fn import_sessions(store: &Store<'_>, dir: &Path) -> Result<Value> {
    absolute(dir, "sessions directory")?;
    let (mut imported, mut skipped, mut empty) = (0, 0, 0);
    let mut failed = Vec::new();
    for path in session_files(dir)? {
        match backfill(store, &path) {
            Ok(Backfill::Imported) => imported += 1,
            Ok(Backfill::Skipped) => skipped += 1,
            Ok(Backfill::Empty) => empty += 1,
            Err(e) => {
                failed.push(json!({"path": path.display().to_string(), "error": e.to_string()}))
            }
        }
    }
    Ok(json!({"imported": imported, "skipped": skipped, "empty": empty, "failed": failed}))
}

enum Backfill {
    Imported,
    Skipped,
    Empty,
}

fn backfill(store: &Store<'_>, path: &Path) -> Result<Backfill> {
    let Some(session) = read_session(path)? else {
        return Ok(Backfill::Empty);
    };
    if transcript::is_captured(store, &session.id)? {
        return Ok(Backfill::Skipped);
    }
    session.capture(store, "backfill")?;
    Ok(Backfill::Imported)
}

/// What [`sync_session`] did with one transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSync {
    /// The session had no capture; one was written.
    Captured,
    /// The file holds more messages than the capture; it was refreshed.
    Refreshed,
    /// The capture already holds at least this many messages, which is the
    /// usual case once the hook has run.
    Unchanged,
    /// The transcript has no messages.
    Empty,
}

/// Capture, or refresh, the session in the Claude Code transcript at
/// `path`: what the folder watcher runs for a settled session file.
///
/// Converges on the hook's capture rather than beside it: both are keyed by
/// the session id, so a session the hook already saved at this length is
/// left alone, and one that grew past it is refreshed in place. A first
/// capture is stamped only with where the transcript says it ran, as a
/// backfill is.
pub fn sync_session(store: &Store<'_>, path: &Path) -> Result<SessionSync> {
    let Some(session) = read_session(path)? else {
        return Ok(SessionSync::Empty);
    };
    let outcome = match transcript::captured_messages(store, &session.id)? {
        Some(held) if held >= session.parsed.messages => return Ok(SessionSync::Unchanged),
        Some(_) => SessionSync::Refreshed,
        None => SessionSync::Captured,
    };
    session.capture(store, "watch")?;
    Ok(outcome)
}

/// A transcript file read and parsed, with the session id it is keyed by.
struct SessionFile {
    id: String,
    raw: String,
    parsed: transcript::TranscriptSummary,
}

impl SessionFile {
    fn capture(&self, store: &Store<'_>, reason: &str) -> Result<()> {
        let context = transcript_context(&self.parsed);
        let request = TranscriptRequest {
            session_id: &self.id,
            cwd: None,
            reason: Some(reason),
            context: Some(&context),
        };
        transcript::capture_transcript(store, &self.raw, &request)?;
        Ok(())
    }
}

/// The session at `path`, or `None` when its transcript has no messages.
fn read_session(path: &Path) -> Result<Option<SessionFile>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| StoreError::Invalid(format!("cannot read {}: {e}", path.display())))?;
    let parsed = transcript::parse(&raw);
    if parsed.messages == 0 {
        return Ok(None);
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // Trimmed once, here, as the capture trims it: the lookup and the
    // capture must agree on the key, or a padded id overwrites the hook's.
    let id = parsed
        .session_id
        .as_deref()
        .unwrap_or(stem)
        .trim()
        .to_string();
    Ok(Some(SessionFile { id, raw, parsed }))
}

/// Where a transcript says its session ran, and nothing else: its cwd and
/// branch, and the project that cwd names. Neither this host's git nor the
/// importing process's directory is consulted: the session's sha and remote
/// are not recorded, and today's checkout of the same path says nothing
/// about them.
fn transcript_context(parsed: &transcript::TranscriptSummary) -> crate::models::WriteContext {
    crate::models::WriteContext {
        project: parsed.cwd.as_deref().and_then(last_segment),
        git_branch: parsed.git_branch.clone(),
        cwd: parsed.cwd.clone(),
        ..Default::default()
    }
}

/// The last segment of a path recorded on any machine. Both separators
/// count: to a Unix `Path`, `C:\code\proj` is a single segment.
fn last_segment(path: &str) -> Option<String> {
    path.rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// `dir/*/*.jsonl`, sorted: one folder per project, one file per session.
/// Deeper files (a session's subagent transcripts) are not sessions.
pub(crate) fn session_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let read = |d: &Path| {
        std::fs::read_dir(d)
            .map_err(|e| StoreError::Invalid(format!("cannot read {}: {e}", d.display())))
    };
    let mut files = Vec::new();
    for project in read(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
    {
        files.extend(
            read(&project)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl")),
        );
    }
    files.sort();
    Ok(files)
}

/// What the `context` subcommand was asked for.
#[derive(Debug, Clone, Default)]
pub struct ContextArgs<'a> {
    pub cwd: Option<&'a Path>,
    pub project: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub prompt: Option<&'a str>,
    /// Tokens; `0` means unlimited.
    pub budget: usize,
    pub hits_only: bool,
}

/// `{"brief", "context"}`: the brief's JSON and its Markdown, each within
/// the budget on its own. A caller prints one of them, never both.
fn brief_json(brief: &Brief) -> Result<Value> {
    let data = serde_json::to_value(brief).map_err(|e| StoreError::Invalid(e.to_string()))?;
    Ok(json!({ "brief": data, "context": context_brief::render_markdown(brief) }))
}

/// A session brief (see [`context_brief`]), as [`brief_json`].
/// The project and branch default to the ones `cwd` is in.
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
    let request = ContextRequest {
        mode: if args.hits_only {
            Mode::Prompt
        } else {
            Mode::Session
        },
        project: project.as_deref(),
        branch: branch.as_deref(),
        prompt: args.prompt,
        since: None,
        budget: args.budget,
    };
    match context_brief::build(store, &request)? {
        Outcome::Brief(brief) => brief_json(&brief),
        // Session and prompt briefs never refuse a project.
        Outcome::UnknownProject { name, .. } => {
            Err(StoreError::Invalid(format!("unknown project {name:?}")))
        }
    }
}

/// A project brief (`rusty-remind-me brief`), as [`brief_json`]. An
/// unknown project is an error naming the closest known ones.
pub fn brief(
    store: &Store<'_>,
    project: &str,
    since: Option<&str>,
    budget: usize,
) -> Result<Value> {
    context_brief::check_token_budget(budget)?;
    let request = ContextRequest {
        mode: Mode::Project,
        project: Some(project),
        since,
        budget,
        ..ContextRequest::default()
    };
    match context_brief::build(store, &request)? {
        Outcome::Brief(brief) => brief_json(&brief),
        Outcome::UnknownProject { name, suggestions } => Err(StoreError::Invalid(
            context_brief::unknown_project_message(&name, &suggestions),
        )),
    }
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
    fn import_sessions_backfills_once_and_reports_every_file() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        assert!(import_sessions(&store, Path::new("rel")).is_err());
        let root =
            std::env::temp_dir().join(format!("rrm_backfill_{}", uuid::Uuid::new_v4().simple()));
        let project = root.join("C--Users-nano-code-quokka");
        std::fs::create_dir_all(project.join("s-win").join("subagents")).unwrap();
        // A Windows transcript: CRLF lines, a `C:\` cwd, a meta line, a tool result.
        let envelope = |v: Value| format!("{v}\r\n");
        let windows = [
            json!({"type": "user", "sessionId": "s-win", "cwd": r"C:\Users\nano\code\quokka",
                "isMeta": true, "message": {"role": "user", "content": "hook context"}}),
            json!({"type": "user", "sessionId": "s-win", "cwd": r"C:\Users\nano\code\quokka",
                "gitBranch": "feat/x", "message": {"role": "user", "content": "fix the build"}}),
            json!({"type": "assistant", "sessionId": "s-win", "message": {"role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo b"}}]}}),
            json!({"type": "user", "sessionId": "s-win", "message": {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}),
            json!({"type": "assistant", "sessionId": "s-win",
                "message": {"role": "assistant", "content": "Fixed."}}),
        ]
        .map(envelope)
        .concat();
        std::fs::write(project.join("s-win.jsonl"), windows).unwrap();
        // No sessionId: the file stem names it.
        let bare = json!({"type": "user", "message": {"role": "user", "content": "hi"}});
        std::fs::write(project.join("s-bare.jsonl"), bare.to_string()).unwrap();
        std::fs::write(project.join("s-empty.jsonl"), "{\"type\":\"summary\"}\n").unwrap();
        std::fs::write(project.join("s-bad.jsonl"), [0xff, 0xfe]).unwrap();
        std::fs::write(
            project.join("s-win").join("subagents").join("a.jsonl"),
            bare.to_string(),
        )
        .unwrap();
        std::fs::write(project.join("notes.txt"), "not a session").unwrap();

        let first = import_sessions(&store, &root).unwrap();
        assert_eq!(
            (&first["imported"], &first["skipped"], &first["empty"]),
            (&json!(2), &json!(0), &json!(1))
        );
        let failed = first["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert!(failed[0]["path"].as_str().unwrap().ends_with("s-bad.jsonl"));

        let memories = episodes::session_memories(&store, "s-win").unwrap();
        assert_eq!(memories.len(), 2, "one dialog and summary pair");
        assert!(memories
            .iter()
            .all(|m| m.project.as_deref() == Some("quokka")));
        assert!(memories
            .iter()
            .all(|m| m.git_branch.as_deref() == Some("feat/x")));
        assert!(memories
            .iter()
            .all(|m| m.git_remote.is_none() && m.git_sha.is_none()));
        let dialog = memories
            .iter()
            .find(|m| m.metadata["type"] == "dialog")
            .unwrap();
        assert!(!dialog.content.contains("hook context") && dialog.content.contains("Bash"));
        assert_eq!(dialog.metadata["reason"], "backfill");
        // No cwd: nothing about the importing process is stamped instead.
        let bare_memories = episodes::session_memories(&store, "s-bare").unwrap();
        assert_eq!(bare_memories.len(), 2);
        assert!(bare_memories.iter().all(|m| m.project.is_none()
            && m.cwd.is_none()
            && m.git_remote.is_none()
            && m.git_branch.is_none()
            && m.git_sha.is_none()));

        let again = import_sessions(&store, &root).unwrap();
        assert_eq!(
            (&again["imported"], &again["skipped"]),
            (&json!(0), &json!(2))
        );
        assert!(import_sessions(&store, &root.join("missing")).is_err());
    }

    #[test]
    fn a_backfill_ignores_the_git_of_a_local_cwd_and_keeps_the_hooks_capture() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let root =
            std::env::temp_dir().join(format!("rrm_backfill_{}", uuid::Uuid::new_v4().simple()));
        // A cwd that exists here, in another repository than the importer's.
        let repo = root.join("wombat");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| crate::worklog::git(&repo, args);
        git(&["init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://example.com/other/wombat.git",
        ]);
        let folder = root.join("projects").join("-wombat");
        std::fs::create_dir_all(&folder).unwrap();
        let cwd = repo.display().to_string();
        let local = json!({"type": "user", "sessionId": "s-local", "cwd": cwd,
            "message": {"role": "user", "content": "hello"}});
        std::fs::write(folder.join("s-local.jsonl"), local.to_string()).unwrap();

        // The hook captured `s1`; a padded id in a transcript is the same session.
        let hook = json!({"type": "user", "sessionId": "s1",
            "message": {"role": "user", "content": "the hook's text"}});
        std::fs::write(root.join("hook.jsonl"), hook.to_string()).unwrap();
        capture_transcript(&store, &root.join("hook.jsonl"), "s1", Some(&repo), None).unwrap();
        let padded = json!({"type": "user", "sessionId": " s1 ",
            "message": {"role": "user", "content": "an older copy"}});
        std::fs::write(folder.join("padded.jsonl"), padded.to_string()).unwrap();

        let result = import_sessions(&store, &root.join("projects")).unwrap();
        assert_eq!(
            (&result["imported"], &result["skipped"]),
            (&json!(1), &json!(1))
        );
        for m in episodes::session_memories(&store, "s-local").unwrap() {
            assert_eq!(m.project.as_deref(), Some("wombat"));
            assert_eq!(m.cwd.as_deref(), Some(cwd.as_str()));
            assert_eq!((m.git_remote, m.git_branch, m.git_sha), (None, None, None));
        }
        let kept = episodes::session_memories(&store, "s1").unwrap();
        assert_eq!(kept.len(), 2);
        let dialog = kept
            .iter()
            .find(|m| m.metadata["type"] == "dialog")
            .unwrap();
        assert!(
            dialog.content.contains("the hook's text"),
            "{}",
            dialog.content
        );
    }

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
        let shown: usize = brief["brief"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["items"].as_array().unwrap().len())
            .sum();
        assert!(shown >= 1, "{brief}");
        assert!(brief["context"]
            .as_str()
            .unwrap()
            .starts_with("# Memory context"));

        let unknown = super::brief(&store, "nowhere", None, 1500).unwrap_err();
        assert!(unknown.to_string().contains("no project"), "{unknown}");
        assert!(super::brief(&store, "nowhere", None, 1).is_err());
        // The CLI prints `brief` or `context`, each within the budget.
        let tight = context(
            &store,
            &ContextArgs {
                budget: 200,
                ..Default::default()
            },
        )
        .unwrap();
        for printed in [
            tight["brief"].to_string(),
            tight["context"].as_str().unwrap().to_string(),
        ] {
            assert!(
                crate::retrieval::estimated_content_tokens(&printed) <= 200,
                "{printed}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
