//! Capturing a Claude Code session transcript as ONE capture per session.
//!
//! A hook fires at every `Stop`, `PreCompact` and `SessionEnd`; each firing
//! hands over the whole transcript so far. The first creates the dialog and
//! summary pair ([`crate::capture::auto_capture_stamped`]); every later one
//! replaces that pair's text in place (a `recapture` revision records what
//! it replaced), so a long session is one capture, not one per turn.
//!
//! Parsing reuses the chat importer's envelope reader
//! ([`crate::importer::extract_messages`]), so the transcript's text, times
//! and tool calls are read the same way an import reads them.

use crate::capture::{auto_capture_stamped, get_capture, CaptureStamp};
use crate::db::memories::{Memories, MemoryEdit};
use crate::db::{Result, Store, StoreError};
use crate::history::{capture_revision, TrackedChanges};
use crate::importer::{extract_messages, ChatMessage};
use crate::models::AutoCaptureInput;
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::Path;

/// Title length, matching the brief: the first user message, truncated.
pub const TITLE_CHARS: usize = 120;
/// Each of "First ask" / "Last reply" in the summary is cut to this.
pub const SUMMARY_PART_CHARS: usize = 500;
/// The dialog keeps at most this many characters, the most recent ones: a
/// transcript can run to megabytes, and what was said last is what a later
/// session needs.
pub const MAX_DIALOG_CHARS: usize = 200_000;
const TRUNCATION_MARKER: &str = "[earlier messages truncated]\n\n";

/// What a transcript says, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSummary {
    /// Messages with text or a tool call.
    pub messages: usize,
    pub first_user: Option<String>,
    pub last_assistant: Option<String>,
    /// Tool name and how many calls, most used first.
    pub tools: Vec<(String, usize)>,
    /// The verbatim dialog, one `**role:** text` block per message.
    pub dialog: String,
    /// Session id the envelopes carry, if any.
    pub session_id: Option<String>,
    /// The cwd the first envelope carried.
    pub cwd: Option<String>,
}

fn is_user(role: &str) -> bool {
    matches!(role, "user" | "human")
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// Read a Claude Code JSONL transcript. A malformed line is skipped, as the
/// importer skips one: a half-written last line must not lose the session.
pub fn parse(raw: &str) -> TranscriptSummary {
    let messages: Vec<ChatMessage> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        // Hook-injected context, not something the user or model said.
        .filter(|v| v.get("isMeta").and_then(|m| m.as_bool()) != Some(true))
        .flat_map(|v| extract_messages(&v))
        .collect();

    let mut tool_counts: BTreeMap<String, usize> = BTreeMap::new();
    for tool in messages.iter().flat_map(|m| &m.tool_use) {
        *tool_counts.entry(tool.name.clone()).or_default() += 1;
    }
    let mut tools: Vec<(String, usize)> = tool_counts.into_iter().collect();
    tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    TranscriptSummary {
        messages: messages.len(),
        first_user: messages
            .iter()
            .find(|m| is_user(&m.role) && !m.content.is_empty())
            .map(|m| m.content.clone()),
        last_assistant: messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.is_empty())
            .map(|m| m.content.clone()),
        tools,
        dialog: render_dialog(&messages),
        session_id: messages.iter().find_map(|m| m.session_id.clone()),
        cwd: messages.iter().find_map(|m| m.cwd.clone()),
    }
}

fn render_dialog(messages: &[ChatMessage]) -> String {
    let dialog = messages
        .iter()
        .map(|m| {
            let mut block = format!("**{}:** {}", m.role, m.content);
            for line in m.tool_lines() {
                block.push('\n');
                block.push_str(&line);
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let total = dialog.chars().count();
    if total <= MAX_DIALOG_CHARS {
        return dialog;
    }
    let tail: String = dialog.chars().skip(total - MAX_DIALOG_CHARS).collect();
    format!("{TRUNCATION_MARKER}{tail}")
}

impl TranscriptSummary {
    /// The summary half's title: the first user message, one line, cut.
    pub fn title(&self) -> String {
        let one_line = self
            .first_user
            .as_deref()
            .unwrap_or("Session transcript")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        cut(&one_line, TITLE_CHARS)
    }

    /// The summary half's body.
    pub fn summary(&self) -> String {
        let tools = if self.tools.is_empty() {
            "none".to_string()
        } else {
            self.tools
                .iter()
                .map(|(name, n)| format!("{name} x{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "First ask: {}\nLast reply: {}\nMessages: {}\nTools used: {}",
            cut(self.first_user.as_deref().unwrap_or("(none)"), SUMMARY_PART_CHARS),
            cut(self.last_assistant.as_deref().unwrap_or("(none)"), SUMMARY_PART_CHARS),
            self.messages,
            tools
        )
    }
}

/// Who is capturing, and from where.
#[derive(Debug, Clone)]
pub struct TranscriptRequest<'a> {
    pub session_id: &'a str,
    pub cwd: Option<&'a Path>,
    /// `stop`, `precompact` or `end`; kept in metadata.
    pub reason: Option<&'a str>,
}

/// The ids the CLI prints.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptOutcome {
    pub capture_id: String,
    pub dialog_id: String,
    pub summary_id: String,
    pub messages: usize,
}

fn with_keys(base: &serde_json::Value, pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut meta = base.as_object().cloned().unwrap_or_default();
    for (key, value) in pairs {
        meta.insert((*key).to_string(), value.clone());
    }
    serde_json::Value::Object(meta)
}

/// Write or refresh the session's capture from `raw` transcript text.
///
/// Errors with `Invalid` for an empty session id or a transcript with no
/// messages, so a hook firing before the first prompt stores nothing.
pub fn capture_transcript(
    store: &Store<'_>,
    raw: &str,
    request: &TranscriptRequest<'_>,
) -> Result<TranscriptOutcome> {
    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        return Err(StoreError::Invalid("session id must not be empty".into()));
    }
    let parsed = parse(raw);
    if parsed.messages == 0 {
        return Err(StoreError::Invalid("transcript has no messages".into()));
    }
    let cwd = request.cwd.map(Path::to_path_buf).or_else(|| parsed.cwd.as_deref().map(Into::into));
    let context = match &cwd {
        Some(dir) => crate::episodes::write_context(dir, Some(session_id)),
        None => crate::models::WriteContext {
            session_id: Some(session_id.to_string()),
            ..Default::default()
        },
    };

    if let Some(existing) = existing_dialog(store, session_id)? {
        return recapture(store, &existing, &parsed, request.reason);
    }

    let mut tags = vec!["transcript".to_string()];
    tags.extend(context.project.clone());
    let input = AutoCaptureInput {
        conversation: parsed.dialog.clone(),
        summary: parsed.summary(),
        title: parsed.title(),
        tags,
        category: "conversation".to_string(),
        metadata: with_keys(
            &serde_json::json!({}),
            &[
                ("messages", parsed.messages.into()),
                ("reason", request.reason.into()),
            ],
        ),
    };
    let stamp = CaptureStamp {
        context,
        written_by: Some("hook".into()),
        capture_method: Some("auto".into()),
    };
    let made = auto_capture_stamped(store, &input, &stamp)?;
    Ok(TranscriptOutcome {
        capture_id: made.capture_id,
        dialog_id: made.dialog_id,
        summary_id: made.summary_id,
        messages: parsed.messages,
    })
}

/// The dialog half already captured for `session_id`, if any.
fn existing_dialog(store: &Store<'_>, session_id: &str) -> Result<Option<crate::models::Memory>> {
    Ok(crate::episodes::session_memories(store, session_id)?
        .into_iter()
        .find(|m| {
            m.capture_id.is_some()
                && m.metadata.get("type").and_then(|t| t.as_str()) == Some("dialog")
        }))
}

/// Replace both halves' text with `parsed`, recording what was replaced.
fn recapture(
    store: &Store<'_>,
    dialog: &crate::models::Memory,
    parsed: &TranscriptSummary,
    reason: Option<&str>,
) -> Result<TranscriptOutcome> {
    let capture_id = dialog.capture_id.clone().ok_or(StoreError::NotFound)?;
    let capture = get_capture(store, &capture_id)?.ok_or(StoreError::NotFound)?;
    let summary = capture.summary.ok_or(StoreError::NotFound)?;
    let title = parsed.title();
    let pairs = [
        ("title", serde_json::json!(title)),
        ("messages", serde_json::json!(parsed.messages)),
        ("reason", serde_json::json!(reason)),
    ];
    replace_text(store, dialog, &parsed.dialog, &with_keys(&dialog.metadata, &pairs))?;
    replace_text(store, &summary, &parsed.summary(), &with_keys(&summary.metadata, &pairs))?;
    Ok(TranscriptOutcome {
        capture_id,
        dialog_id: dialog.id.clone(),
        summary_id: summary.id,
        messages: parsed.messages,
    })
}

/// Edit a memory's content and metadata, writing a `recapture` revision of
/// what it held and re-embedding best-effort, as an update does.
fn replace_text(
    store: &Store<'_>,
    memory: &crate::models::Memory,
    content: &str,
    metadata: &serde_json::Value,
) -> Result<()> {
    let tracked = TrackedChanges {
        content: Some(content.to_string()),
        metadata_json: Some(serde_json::to_string(metadata).unwrap_or_else(|_| "{}".into())),
        ..TrackedChanges::default()
    };
    capture_revision(store, &memory.id, &tracked, Some("recapture"))?;
    Memories::new(store).apply_edit(
        &memory.id,
        &MemoryEdit {
            content: Some(content.to_string()),
            metadata: Some(metadata.clone()),
            ..MemoryEdit::at(Utc::now().to_rfc3339())
        },
    )?;
    if let Some(embedder) = crate::embedder::available_embedder() {
        let _ = crate::vectors::embed_and_store(store, &*embedder, &memory.id, content);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn line(role: &str, session: &str, text: &str) -> String {
        serde_json::json!({
            "type": role, "sessionId": session, "uuid": format!("u-{text}"),
            "timestamp": "2026-10-04T10:00:00.000Z", "cwd": "/work/quokka",
            "message": {"role": role, "content": text}
        })
        .to_string()
    }

    fn tool_line() -> String {
        serde_json::json!({
            "type": "assistant", "sessionId": "s1", "cwd": "/work/quokka",
            "timestamp": "2026-10-04T10:00:05.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "running tests"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test -p remind_me_core"}}
            ]}
        })
        .to_string()
    }

    fn transcript(extra: &str) -> String {
        [
            line("user", "s1", "fix the flaky test please"),
            tool_line(),
            "{ not json".to_string(),
            line("assistant", "s1", "done, it was a race"),
            extra.to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn parse_reads_ask_reply_tools_and_skips_bad_lines() {
        let p = parse(&transcript(""));
        assert_eq!(p.messages, 3);
        assert_eq!(p.first_user.as_deref(), Some("fix the flaky test please"));
        assert_eq!(p.last_assistant.as_deref(), Some("done, it was a race"));
        assert_eq!(p.tools, vec![("Bash".to_string(), 1)]);
        assert_eq!(p.session_id.as_deref(), Some("s1"));
        assert_eq!(p.cwd.as_deref(), Some("/work/quokka"));
        assert!(p.dialog.contains("tool: Bash — cargo test -p remind_me_core"));
        let summary = p.summary();
        assert!(summary.starts_with("First ask: fix the flaky test please\nLast reply: done"));
        assert!(summary.ends_with("Messages: 3\nTools used: Bash x1"));
    }

    #[test]
    fn parse_truncates_title_and_dialog_and_skips_meta() {
        let long = "x".repeat(300);
        let meta = serde_json::json!({"isMeta": true, "message": {"role": "user", "content": "injected"}});
        let p = parse(&format!("{}\n{}", line("user", "s", &long), meta));
        assert_eq!(p.messages, 1);
        assert_eq!(p.title().chars().count(), TITLE_CHARS);
        let big = "y".repeat(MAX_DIALOG_CHARS + 50);
        let p = parse(&line("user", "s", &big));
        assert!(p.dialog.starts_with(TRUNCATION_MARKER));
        assert_eq!(p.dialog.chars().count(), TRUNCATION_MARKER.chars().count() + MAX_DIALOG_CHARS);
        assert_eq!(parse("").messages, 0);
    }

    fn request(reason: &str) -> TranscriptRequest<'_> {
        TranscriptRequest {
            session_id: "s1",
            cwd: Some(Path::new("/work/quokka")),
            reason: Some(reason),
        }
    }

    #[test]
    fn one_capture_per_session_and_recapture_replaces_in_place() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let first = capture_transcript(&store, &transcript(""), &request("stop")).unwrap();
        assert_eq!(first.messages, 3);

        let grown = transcript(&line("user", "s1", "and one more thing"));
        let second = capture_transcript(&store, &grown, &request("end")).unwrap();
        assert_eq!(second.capture_id, first.capture_id);
        assert_eq!(second.dialog_id, first.dialog_id);
        assert_eq!(second.summary_id, first.summary_id);
        assert_eq!(second.messages, 4);

        let memories = crate::episodes::session_memories(&store, "s1").unwrap();
        assert_eq!(memories.len(), 2, "still one pair");
        let dialog = memories.iter().find(|m| m.id == first.dialog_id).unwrap();
        assert!(dialog.content.contains("and one more thing"));
        assert_eq!(dialog.written_by, "hook");
        assert_eq!(dialog.capture_method, "auto");
        assert_eq!(dialog.session_id.as_deref(), Some("s1"));
        assert_eq!(dialog.cwd.as_deref(), Some("/work/quokka"));
        assert_eq!(dialog.project.as_deref(), Some("quokka"));
        assert_eq!(dialog.metadata["reason"], "end");
        let summary = memories.iter().find(|m| m.id == first.summary_id).unwrap();
        assert!(summary.content.contains("Messages: 4"));

        let revisions = crate::db::history::Revisions::new(&store)
            .list(&first.dialog_id, 10)
            .unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].revision_reason.as_deref(), Some("recapture"));
        assert!(crate::db::sessions::Sessions::new(&store).get("s1").unwrap().is_some());
    }

    #[test]
    fn sessions_do_not_share_a_capture_and_bad_input_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let a = capture_transcript(&store, &transcript(""), &request("stop")).unwrap();
        let other = TranscriptRequest { session_id: "s2", ..request("stop") };
        let b = capture_transcript(&store, &transcript(""), &other).unwrap();
        assert_ne!(a.capture_id, b.capture_id);

        assert!(capture_transcript(&store, "", &request("stop")).is_err());
        let blank = TranscriptRequest { session_id: " ", ..request("stop") };
        assert!(capture_transcript(&store, &transcript(""), &blank).is_err());
    }
}
