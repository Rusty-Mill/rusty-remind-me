//! Capturing a Claude Code session transcript as ONE capture per session.
//!
//! A hook fires at every `Stop`, `PreCompact` and `SessionEnd`; each firing
//! hands over the whole transcript so far. The first creates the dialog and
//! summary pair ([`crate::capture::auto_capture_stamped`]); every later one
//! refreshes the summary in place (a `recapture` revision records what it
//! replaced), so a long session is one capture, not one per turn. The dialog
//! half is never rewritten: dialogs are read-only (`crate::dialog`).
//!
//! Parsing reuses the chat importer's envelope reader
//! ([`crate::importer::extract_messages_with_tools`]), so the transcript's text, times
//! and tool calls are read the same way an import reads them.

use crate::capture::{auto_capture_stamped, get_capture, CaptureStamp};
use crate::db::memories::{Memories, MemoryEdit, NewMemory};
use crate::db::{Result, Store, StoreError};
use crate::history::{capture_revision, TrackedChanges};
use crate::importer::{extract_messages_with_tools, ChatMessage, ToolUseSummary};
use crate::models::{AutoCaptureInput, WriteContext};
use chrono::Utc;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::Path;

/// Title length, matching the brief: the first user message, truncated.
pub const TITLE_CHARS: usize = 120;
/// Each of "First ask" / "Last reply" in the summary is cut to this.
pub const SUMMARY_PART_CHARS: usize = 500;
/// The dialog keeps at most this many characters, the most recent ones: a
/// transcript can run to megabytes, and what was said last is what a later
/// session needs.
pub const MAX_DIALOG_CHARS: usize = 200_000;
/// Largest transcript a remote caller may send in one capture, counted as
/// it travels: its [`encoded_len`] as a JSON string, since escaping can
/// nearly double quote-heavy JSONL. A longer session sends its tail: the
/// dialog keeps only the newest [`MAX_DIALOG_CHARS`] anyway. The remaining
/// 1 MiB of the remote server's 4 MiB request body is the envelope's.
pub const MAX_TRANSCRIPT_BYTES: usize = 3 * 1024 * 1024;
/// Bytes `text` takes once escaped into a JSON string, quotes excluded:
/// what it costs in a request body.
pub fn encoded_len(text: &str) -> usize {
    text.bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

const TRUNCATION_MARKER: &str = "[earlier messages truncated]\n\n";

/// Names the tools whose call posts a message the user reads, as
/// `tool=argument` pairs separated by commas. Unset means
/// [`MessageTools::defaults`]; set but blank means none.
pub const MESSAGE_TOOLS_ENV: &str = "REMIND_ME_MESSAGE_TOOLS";

/// A tool whose call posts a message the user reads, and the argument that
/// holds the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTool {
    pub name: String,
    pub arg: String,
}

/// The tools whose posted message a capture keeps in full and counts as a
/// reply. A session that talks through such a tool (a claude.ai thread
/// posting with `mcp__hearthbot__post_message`) says its substance there,
/// not in plain assistant text. Every other tool keeps its short preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTools(pub Vec<MessageTool>);

impl MessageTools {
    /// The thread reply tool of claude.ai projects.
    pub fn defaults() -> Self {
        Self(vec![MessageTool {
            name: "mcp__hearthbot__post_message".into(),
            arg: "text".into(),
        }])
    }

    /// The list a [`MESSAGE_TOOLS_ENV`] value names: `None` (unset) is the
    /// defaults, a blank value is none, anything else must be well formed.
    pub fn from_setting(value: Option<&str>) -> std::result::Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::defaults());
        };
        if value.trim().is_empty() {
            return Ok(Self(Vec::new()));
        }
        value
            .split(',')
            .map(parse_pair)
            .collect::<std::result::Result<_, _>>()
            .map(Self)
    }

    /// [`Self::from_setting`] of the environment. A malformed value is
    /// reported and the defaults used, so a typo never stops a capture.
    pub fn from_env() -> Self {
        let value = std::env::var(MESSAGE_TOOLS_ENV).ok();
        Self::from_setting(value.as_deref()).unwrap_or_else(|e| {
            eprintln!("rusty-remind-me: {MESSAGE_TOOLS_ENV} ignored: {e}");
            Self::defaults()
        })
    }

    /// The message `call` posted, when it is one of these tools and the
    /// argument holds text.
    fn posted<'a>(&self, call: &'a ToolUseSummary) -> Option<&'a str> {
        let tool = self.0.iter().find(|t| t.name == call.name)?;
        call.input
            .get(&tool.arg)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
    }
}

/// One `tool=argument` pair of a [`MESSAGE_TOOLS_ENV`] value.
fn parse_pair(pair: &str) -> std::result::Result<MessageTool, String> {
    let malformed = || format!("`{}` is not tool=argument", pair.trim());
    let (name, arg) = pair.split_once('=').ok_or_else(malformed)?;
    let (name, arg) = (name.trim(), arg.trim());
    if name.is_empty() || arg.is_empty() || arg.contains('=') {
        return Err(malformed());
    }
    Ok(MessageTool {
        name: name.into(),
        arg: arg.into(),
    })
}

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
    /// The git branch the first envelope carried.
    pub git_branch: Option<String>,
}

fn is_user(role: &str) -> bool {
    matches!(role, "user" | "human")
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// [`parse_with`] the message tools the environment names.
pub fn parse(raw: &str) -> TranscriptSummary {
    parse_with(raw, &MessageTools::from_env())
}

/// Read a Claude Code JSONL transcript. A malformed line is skipped, as the
/// importer skips one: a half-written last line must not lose the session.
/// A message posted through one of `message_tools` is kept in full and is
/// the session's reply wherever it comes last.
pub fn parse_with(raw: &str, message_tools: &MessageTools) -> TranscriptSummary {
    let messages = messages_of(raw);

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
            .filter(|m| m.role == "assistant")
            .find_map(|m| reply_of(m, message_tools))
            .map(str::to_string),
        tools,
        dialog: render_dialog(&messages, message_tools),
        session_id: messages.iter().find_map(|m| m.session_id.clone()),
        cwd: messages.iter().find_map(|m| m.cwd.clone()),
        git_branch: messages.iter().find_map(|m| m.git_branch.clone()),
    }
}

/// One piece of a message, in the order it was written.
enum Part<'a> {
    Text(&'a str),
    Tool(&'a ToolUseSummary),
}

/// A message's text blocks and tool calls in content order. A message with
/// no text blocks or no calls is its text, then its calls, so text-only
/// messages render exactly as their joined content.
fn parts(message: &ChatMessage) -> Vec<Part<'_>> {
    if message.text_blocks.is_empty() || message.tool_use.is_empty() {
        let text = (!message.content.is_empty()).then_some(Part::Text(&message.content));
        return text
            .into_iter()
            .chain(message.tool_use.iter().map(Part::Tool))
            .collect();
    }
    let mut ordered: Vec<(usize, Part<'_>)> = message
        .text_blocks
        .iter()
        .map(|t| (t.block, Part::Text(&t.text)))
        .chain(message.tool_use.iter().map(|c| (c.block, Part::Tool(c))))
        .collect();
    ordered.sort_by_key(|(block, _)| *block);
    ordered.into_iter().map(|(_, part)| part).collect()
}

/// What an assistant message said last: the last of its text blocks and
/// posted messages, in content order.
fn reply_of<'a>(message: &'a ChatMessage, message_tools: &MessageTools) -> Option<&'a str> {
    parts(message)
        .into_iter()
        .rev()
        .find_map(|part| match part {
            Part::Text(text) => Some(text.trim()),
            Part::Tool(call) => message_tools.posted(call),
        })
}

/// A message as one dialog block, `**role:** ` then its text and tool lines
/// in content order: a message tool's text in full, any other tool's preview.
fn render_message(message: &ChatMessage, message_tools: &MessageTools) -> String {
    let lines: Vec<String> = parts(message)
        .into_iter()
        .map(|part| match part {
            Part::Text(text) => text.to_string(),
            Part::Tool(call) => {
                call.line(message_tools.posted(call).unwrap_or(&call.input_summary))
            }
        })
        .collect();
    let separator = if message.content.is_empty() { "\n" } else { "" };
    format!(
        "**{}:** {separator}{}",
        message.role,
        lines.join("\n").trim()
    )
}

/// The messages of a JSONL transcript, in order. A malformed line is skipped.
fn messages_of(raw: &str) -> Vec<ChatMessage> {
    message_lines(raw).map(|(_, m)| m).collect()
}

/// Each message of a JSONL transcript with the byte range of the line it
/// came from, line ending excluded. The one parse [`messages_of`] uses, so a
/// message's source span cannot drift from the message.
fn message_lines(raw: &str) -> impl Iterator<Item = (Range<usize>, ChatMessage)> + '_ {
    let mut at = 0;
    raw.split_inclusive('\n')
        .map(move |line| {
            let start = at;
            at += line.len();
            let body = line.trim_end_matches(['\n', '\r']);
            (start..start + body.len(), body)
        })
        .filter_map(|(span, body)| {
            let value = serde_json::from_str::<serde_json::Value>(body.trim()).ok()?;
            Some((span, value))
        })
        // Hook-injected context, not something the user or model said.
        .filter(|(_, v)| v.get("isMeta").and_then(|m| m.as_bool()) != Some(true))
        .flat_map(|(span, v)| {
            extract_messages_with_tools(&v)
                .into_iter()
                .map(move |m| (span.clone(), m))
        })
}

use crate::transcript_placement::{
    place, role_of, roles, split_row, Block, Incoming, Placement, Record, Shape, MESSAGE_SEPARATOR,
};

/// Each message of `messages` rendered and scrubbed as a stored dialog
/// holds it, so it can be compared with the record.
fn stored_blocks(messages: &[ChatMessage], message_tools: &MessageTools) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            let (mut tags, mut metadata) = (Vec::new(), serde_json::json!({}));
            crate::boundary::scrub(&render_message(m, message_tools), &mut tags, &mut metadata)
        })
        .collect()
}

/// A dialog row's messages for text placement. A row cut to
/// [`MAX_DIALOG_CHARS`] opens with a fragment; a row that records its
/// message count is checked against it.
fn row_blocks<'a>(row: &'a crate::models::Memory, roles: &HashSet<&str>) -> Vec<Block<'a>> {
    let (content, cut) = match row.content.strip_prefix(TRUNCATION_MARKER) {
        Some(rest) => (rest, true),
        None => (row.content.as_str(), false),
    };
    let shape = Shape {
        cut,
        expected: Some(held_messages(row)).filter(|&n| n > 0),
        roles_listed: row.metadata.get(ROLES_KEY).is_some_and(|r| r.is_array()),
    };
    split_row(content, roles, shape)
}

/// Every message's id, or `None` when any message lacks one.
fn message_ids(messages: &[ChatMessage]) -> Option<Vec<String>> {
    messages.iter().map(|m| m.uuid.clone()).collect()
}

/// The message ids a dialog row lists, when it lists them.
fn listed_ids(row: &crate::models::Memory) -> Option<Vec<String>> {
    let ids = row.metadata.get(MESSAGE_IDS_KEY)?.as_array()?;
    ids.iter().map(|v| v.as_str().map(str::to_string)).collect()
}

/// Every id the record's rows hold, or `None` when any row lists none.
fn recorded_ids(rows: &[crate::models::Memory]) -> Option<HashSet<String>> {
    let mut held = HashSet::new();
    for row in rows {
        held.extend(listed_ids(row)?);
    }
    Some(held)
}

/// `metadata` key listing the message ids a dialog row holds.
const MESSAGE_IDS_KEY: &str = "message_ids";

/// `metadata` key listing the roles of the messages a dialog row holds, so
/// the row can be split back into messages whatever a later transcript
/// holds.
const ROLES_KEY: &str = "roles";

/// The distinct roles of `messages`, sorted.
fn roles_of(messages: &[&ChatMessage]) -> Vec<String> {
    let roles: std::collections::BTreeSet<&str> =
        messages.iter().map(|m| m.role.as_str()).collect();
    roles.into_iter().map(str::to_string).collect()
}

/// Every role the record holds: what each row lists, and the role that
/// opens each uncut row (rows written before roles were listed).
fn record_roles(rows: &[crate::models::Memory]) -> HashSet<String> {
    let listed = rows
        .iter()
        .filter_map(|row| row.metadata.get(ROLES_KEY)?.as_array().cloned())
        .flatten()
        .filter_map(|role| role.as_str().map(str::to_string));
    let opening = rows
        .iter()
        .filter(|row| !row.content.starts_with(TRUNCATION_MARKER))
        .filter_map(|row| role_of(&row.content).map(str::to_string));
    listed.chain(opening).collect()
}

/// Whether an unplaced row of `rows` already holds this exact submission,
/// so keeping it again would only add a copy.
///
/// Compared position by position. Two known ids that differ at any
/// position make the submission new turns, however equal the text. When
/// every position has two known ids, the ids decide alone. Otherwise the
/// whole submission must equal the whole row, message for message; a run
/// inside a row is not enough.
fn held_unplaced(
    rows: &[crate::models::Memory],
    roles: &HashSet<&str>,
    blocks: &[String],
    ids: &[Option<String>],
) -> bool {
    rows.iter()
        .filter(|row| is_unplaced(row))
        .any(|row| holds_exactly(row, roles, blocks, ids))
}

fn holds_exactly(
    row: &crate::models::Memory,
    roles: &HashSet<&str>,
    blocks: &[String],
    ids: &[Option<String>],
) -> bool {
    let stored = position_ids(row);
    if stored.is_empty() {
        return same_text(row, roles, blocks);
    }
    if stored.len() != ids.len() {
        return false;
    }
    let pairs = || stored.iter().zip(ids);
    if pairs().any(|(a, b)| matches!((a, b), (Some(a), Some(b)) if a != b)) {
        return false;
    }
    pairs().all(|(a, b)| a.is_some() && b.is_some()) || same_text(row, roles, blocks)
}

fn same_text(row: &crate::models::Memory, roles: &HashSet<&str>, blocks: &[String]) -> bool {
    row_blocks(row, roles)
        .into_iter()
        .eq(blocks.iter().map(|b| Some(b.as_str())))
}

/// The id a dialog row lists for each of its messages, `None` where a
/// message had none; empty when the row lists no ids.
fn position_ids(row: &crate::models::Memory) -> Vec<Option<String>> {
    let Some(ids) = row.metadata.get(MESSAGE_IDS_KEY).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    ids.iter().map(|v| v.as_str().map(str::to_string)).collect()
}

/// Whether a dialog row holds a transcript kept whole because it could not
/// be placed.
fn is_unplaced(row: &crate::models::Memory) -> bool {
    row.metadata
        .get("placement")
        .and_then(|p| p.as_str())
        .is_some_and(|p| p.starts_with("unplaced"))
}

fn render_dialog(messages: &[ChatMessage], message_tools: &MessageTools) -> String {
    let dialog = messages
        .iter()
        .map(|m| render_message(m, message_tools))
        .collect::<Vec<_>>()
        .join(MESSAGE_SEPARATOR);
    let total = dialog.chars().count();
    if total <= MAX_DIALOG_CHARS {
        return dialog;
    }
    let tail: String = dialog.chars().skip(total - MAX_DIALOG_CHARS).collect();
    format!("{TRUNCATION_MARKER}{tail}")
}

impl TranscriptSummary {
    /// The summary half's title: the first user message, redacted, one
    /// line, cut. Redacted first: the title is stored as metadata, which the
    /// content's redaction does not reach.
    pub fn title(&self) -> String {
        let source = crate::boundary::scrub_field(
            self.first_user.as_deref().unwrap_or("Session transcript"),
        );
        let one_line = source.split_whitespace().collect::<Vec<_>>().join(" ");
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
            cut(
                self.first_user.as_deref().unwrap_or("(none)"),
                SUMMARY_PART_CHARS
            ),
            cut(
                self.last_assistant.as_deref().unwrap_or("(none)"),
                SUMMARY_PART_CHARS
            ),
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
    /// Where the session ran, as its caller saw it. Set by a remote caller,
    /// whose cwd and git state are on another machine; `None` probes `cwd`
    /// here.
    pub context: Option<&'a WriteContext>,
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

/// Serializes every transcript capture in this process.
///
/// The hook, the MCP tool and the folder watcher all reach
/// [`capture_transcript`], and the watcher does so from its own thread
/// without the store's lock. Choosing between a first capture and a refresh,
/// writing the dialog and summary pair, and archiving the transcript are
/// separate store calls; interleaved, two first captures make two pairs and
/// a stale refresh lands over a newer one. Holding this across all of it
/// makes each capture one step.
static CAPTURE_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

/// Write or refresh the session's capture from `raw` transcript text.
///
/// Errors with `Invalid` for an empty session id or a transcript with no
/// messages, so a hook firing before the first prompt stores nothing.
pub fn capture_transcript(
    store: &Store<'_>,
    raw: &str,
    request: &TranscriptRequest<'_>,
) -> Result<TranscriptOutcome> {
    let (session_id, parsed) = checked(raw, request)?;
    let _serial = CAPTURE_LOCK.lock();
    let existing = existing_dialog(store, session_id)?;
    write_capture(store, session_id, raw, &parsed, existing, request)
}

/// When [`capture_unless_held`] may replace a session's existing capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Never: a backfill fills in sessions nothing captured.
    Never,
    /// Only with a transcript holding more messages: the folder watcher,
    /// which must not put an older reading over a newer capture.
    IfLonger,
}

/// What [`capture_unless_held`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional {
    /// The session had no capture; one was written.
    First(TranscriptOutcome),
    /// The transcript held more messages than the capture; it was refreshed.
    Refreshed(TranscriptOutcome),
    /// The session's capture, holding this many messages, was left alone.
    Held(usize),
}

/// [`capture_transcript`], unless the session already has a capture that
/// `replace` says to keep: what a writer other than the hook runs. The
/// lookup, the comparison and the write happen under the same lock as every
/// other capture, so the decision cannot go stale before the write.
pub fn capture_unless_held(
    store: &Store<'_>,
    raw: &str,
    request: &TranscriptRequest<'_>,
    replace: Replace,
) -> Result<Conditional> {
    capture_unless_held_pausing(store, raw, request, replace, &|| {})
}

/// [`capture_unless_held`], calling `after_lookup` between reading the
/// existing capture and writing: the point where a second writer would
/// otherwise slip in. For tests that put one there.
#[doc(hidden)]
pub fn capture_unless_held_pausing(
    store: &Store<'_>,
    raw: &str,
    request: &TranscriptRequest<'_>,
    replace: Replace,
    after_lookup: &dyn Fn(),
) -> Result<Conditional> {
    let (session_id, parsed) = checked(raw, request)?;
    let _serial = CAPTURE_LOCK.lock();
    let existing = existing_dialog(store, session_id)?;
    after_lookup();
    // Unplaced rows may repeat turns already held, so they do not make a
    // capture look longer than a newer reading.
    let held = existing
        .as_ref()
        .map(|dialog| placed_by(store, dialog))
        .transpose()?;
    let keep = |held: usize| match replace {
        Replace::Never => true,
        Replace::IfLonger => held >= parsed.messages,
    };
    if let Some(held) = held.filter(|held| keep(*held)) {
        return Ok(Conditional::Held(held));
    }
    let outcome = write_capture(store, session_id, raw, &parsed, existing, request)?;
    Ok(match held {
        Some(_) => Conditional::Refreshed(outcome),
        None => Conditional::First(outcome),
    })
}

/// The trimmed session id and the parsed transcript, or why there is
/// nothing to capture.
fn checked<'r>(raw: &str, request: &TranscriptRequest<'r>) -> Result<(&'r str, TranscriptSummary)> {
    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        return Err(StoreError::Invalid("session id must not be empty".into()));
    }
    let parsed = parse(raw);
    if parsed.messages == 0 {
        return Err(StoreError::Invalid("transcript has no messages".into()));
    }
    Ok((session_id, parsed))
}

/// The live rows holding the dialog of the capture `first` opens: `first`,
/// then its continuation rows in the order they were written.
fn dialog_rows(
    store: &Store<'_>,
    first: &crate::models::Memory,
) -> Result<Vec<crate::models::Memory>> {
    let capture = match first.capture_id.as_deref() {
        Some(id) => get_capture(store, id)?,
        None => None,
    };
    let mut rows = vec![first.clone()];
    if let Some(capture) = capture {
        rows.extend(capture.continuations().into_iter().cloned());
    }
    Ok(rows)
}

/// How many messages the dialog rows of `first`'s capture hold: each row
/// records its own count.
fn held_by(store: &Store<'_>, first: &crate::models::Memory) -> Result<usize> {
    Ok(dialog_rows(store, first)?.iter().map(held_messages).sum())
}

/// How many messages the placed dialog rows of `first`'s capture hold:
/// [`held_by`] without rows kept whole because they could not be placed.
fn placed_by(store: &Store<'_>, first: &crate::models::Memory) -> Result<usize> {
    let rows = dialog_rows(store, first)?;
    Ok(rows
        .iter()
        .filter(|row| !is_unplaced(row))
        .map(held_messages)
        .sum())
}

/// How many messages a captured memory's metadata records; `0` for one
/// written before the count was recorded.
fn held_messages(dialog: &crate::models::Memory) -> usize {
    dialog
        .metadata
        .get("messages")
        .and_then(serde_json::Value::as_u64)
        .map_or(0, |n| n as usize)
}

/// The capture itself, with [`CAPTURE_LOCK`] held by the caller.
fn write_capture(
    store: &Store<'_>,
    session_id: &str,
    raw: &str,
    parsed: &TranscriptSummary,
    existing: Option<crate::models::Memory>,
    request: &TranscriptRequest<'_>,
) -> Result<TranscriptOutcome> {
    let (outcome, keep) = match existing {
        Some(existing) => recapture(store, &existing, raw, parsed, request.reason)?,
        None => first_capture(store, session_id, raw, parsed, request)?,
    };
    // The raw transcript decorates the capture; failing to keep it must not
    // fail the capture, so it is reported and the capture stands.
    if let Err(e) = keep_source(store, session_id, raw, &keep) {
        eprintln!("rusty-remind-me: transcript of session {session_id} not archived: {e}");
    }
    Ok(outcome)
}

/// Where the row a capture wrote takes its raw source from.
enum Keep {
    /// A first capture: the transcript becomes the session's archive, and
    /// the first row's span covers its `messages` messages.
    First { row: String, messages: usize },
    /// A continuation row made from the transcript messages at `indexes`.
    Continuation {
        row: String,
        indexes: Vec<usize>,
        /// Whether the messages were placed. Only a placed transcript may
        /// replace the session's archive, and only when it extends it.
        placed: bool,
    },
    /// A copy the record already holds: nothing was written. These rows
    /// (each a `First`, or a `Continuation` with `placed` false) hold exactly
    /// what the copy holds at those places, so a row whose source was lost
    /// to an archive outage takes it from the copy (#1621).
    Repair(Vec<Keep>),
}

/// Archive what `keep` asks for and record the written row's span. The
/// transcript is scrubbed like the dialog, so a source never returns what
/// the dialog redacted.
///
/// The session's archive (`session:<id>`) holds the transcript the first
/// row came from. It is replaced only by a placed transcript that extends
/// it byte for byte, so the first row's span keeps reading its own bytes.
/// A continuation's source is kept under its own row
/// ([`crate::archive::row_import_id`]): exactly the lines its messages came
/// from, or no source at all when a line it needs also holds a message it
/// does not (fail closed).
fn keep_source(
    store: &Store<'_>,
    session_id: &str,
    raw: &str,
    keep: &Keep,
) -> std::result::Result<(), crate::archive::ArchiveError> {
    if !crate::archive::is_enabled() {
        return Ok(());
    }
    let kept = crate::boundary::scrub_jsonl(raw);
    let source = Source {
        session: crate::archive::session_import_id(session_id),
        spans: message_lines(&kept).map(|(span, _)| span).collect(),
        kept,
    };
    match keep {
        Keep::First { row, messages } => keep_first(store, &source, row, *messages),
        Keep::Continuation {
            row,
            indexes,
            placed,
        } => {
            if *placed && extends(store, &source.session, &source.kept)? {
                crate::archive::replace(store, &source.session, source.kept.as_bytes())?;
            }
            keep_own(store, &source, row, indexes)
        }
        Keep::Repair(rows) => rows
            .iter()
            .try_for_each(|keep| repair(store, &source, keep)),
    }
}

/// A transcript as it is archived: scrubbed, with each message's line span.
struct Source {
    session: String,
    kept: String,
    spans: Vec<Range<usize>>,
}

/// Make the transcript the session's archive and point the first row at
/// its first `messages` messages.
fn keep_first(
    store: &Store<'_>,
    source: &Source,
    row: &str,
    messages: usize,
) -> std::result::Result<(), crate::archive::ArchiveError> {
    crate::archive::replace(store, &source.session, source.kept.as_bytes())?;
    let Some(span) = cover(&source.spans, messages) else {
        return Ok(()); // No messages: nothing to point at.
    };
    crate::archive::record_span(store, row, &source.session, span.start, span.end)?;
    Ok(())
}

/// Keep exactly the lines of the messages at `indexes` under `row`'s own
/// archive, and point the row at them.
fn keep_own(
    store: &Store<'_>,
    source: &Source,
    row: &str,
    indexes: &[usize],
) -> std::result::Result<(), crate::archive::ArchiveError> {
    let Some(slices) = slices(&source.spans, indexes) else {
        return Err(crate::archive::ArchiveError::Io(std::io::Error::other(
            format!(
                "row {row}: a source line also holds a message the row does not; \
                 no source recorded"
            ),
        )));
    };
    let own: Vec<&str> = slices.iter().map(|r| &source.kept[r.clone()]).collect();
    let own = own.join("\n");
    let import_id = crate::archive::row_import_id(row);
    crate::archive::replace(store, &import_id, own.as_bytes())?;
    crate::archive::record_span(store, row, &import_id, 0, own.len())?;
    Ok(())
}

/// Give a held row that has no source one from the resent copy (#1621). A
/// row with a source is left alone. The first row takes the copy as the
/// session's archive only where that archive is missing or the copy
/// extends it byte for byte, so every span already recorded keeps reading
/// its own bytes; a continuation takes its own lines, as when written.
fn repair(
    store: &Store<'_>,
    source: &Source,
    keep: &Keep,
) -> std::result::Result<(), crate::archive::ArchiveError> {
    let row = match keep {
        Keep::First { row, .. } | Keep::Continuation { row, .. } => row,
        Keep::Repair(_) => return Ok(()),
    };
    if crate::archive::source_for(store, row, true)?.is_some() {
        return Ok(());
    }
    match keep {
        Keep::First { row, messages } => {
            // A line shared with a message past the row would claim a turn
            // the row does not hold.
            let own: Vec<usize> = (0..*messages).collect();
            if slices(&source.spans, &own).is_none() {
                return Ok(());
            }
            let archived = crate::archive::archived_bytes(store, &source.session)?;
            if archived.is_some_and(|old| !source.kept.as_bytes().starts_with(&old)) {
                return Ok(());
            }
            keep_first(store, source, row, *messages)
        }
        Keep::Continuation { row, indexes, .. } => keep_own(store, source, row, indexes),
        Keep::Repair(_) => Ok(()),
    }
}

/// Whether `kept` starts with the bytes archived under `import_id`. An
/// archive that cannot be read is not extended: its rows' spans would point
/// at bytes nobody checked.
fn extends(
    store: &Store<'_>,
    import_id: &str,
    kept: &str,
) -> std::result::Result<bool, crate::archive::ArchiveError> {
    Ok(crate::archive::archived_bytes(store, import_id)?
        .is_some_and(|old| kept.as_bytes().starts_with(&old)))
}

/// The bytes from the first line to the line of the `count`th message.
fn cover(spans: &[Range<usize>], count: usize) -> Option<Range<usize>> {
    let first = spans.first()?;
    let last = spans.get(count.checked_sub(1)?)?;
    Some(first.start..last.end)
}

/// The distinct source lines of the messages at `indexes`, in transcript
/// order, or `None` when one of them also holds a message not selected:
/// those bytes would claim a turn the row does not hold.
///
/// One pass: a line's messages are adjacent in `spans`, so each run of
/// equal spans is one line, kept when all its messages are selected and
/// refused when only some are.
fn slices(spans: &[Range<usize>], indexes: &[usize]) -> Option<Vec<Range<usize>>> {
    let mut selected = vec![false; spans.len()];
    for &i in indexes {
        *selected.get_mut(i)? = true;
    }
    let mut lines = Vec::new();
    let mut at = 0;
    while at < spans.len() {
        let run = spans[at..]
            .iter()
            .take_while(|span| **span == spans[at])
            .count();
        let chosen = selected[at..at + run].iter().filter(|s| **s).count();
        match chosen {
            0 => {}
            n if n == run => lines.push(spans[at].clone()),
            _ => return None,
        }
        at += run;
    }
    Some(lines)
}

/// Where the session ran: the caller's own context when given, else what
/// probing its cwd here finds.
fn session_context(
    session_id: &str,
    parsed: &TranscriptSummary,
    request: &TranscriptRequest<'_>,
) -> WriteContext {
    if let Some(given) = request.context {
        return WriteContext {
            session_id: Some(session_id.to_string()),
            ..given.clone()
        };
    }
    let cwd = request
        .cwd
        .map(Path::to_path_buf)
        .or_else(|| parsed.cwd.as_deref().map(Into::into));
    match &cwd {
        Some(dir) => crate::episodes::write_context(dir, Some(session_id)),
        None => WriteContext {
            session_id: Some(session_id.to_string()),
            ..Default::default()
        },
    }
}

/// Each message's id in `raw` (`None` where it has none), when the dialog
/// holds every message: a dialog cut to [`MAX_DIALOG_CHARS`] lists none, so
/// ids are never trusted for turns that were cut. Placement by id needs
/// every entry known; text placement uses the known ones to refuse a match
/// between different turns.
fn whole_ids(raw: &str, parsed: &TranscriptSummary) -> Option<Vec<Option<String>>> {
    if parsed.dialog.starts_with(TRUNCATION_MARKER) {
        return None;
    }
    Some(messages_of(raw).into_iter().map(|m| m.uuid).collect())
}

/// Create the session's dialog and summary pair.
fn first_capture(
    store: &Store<'_>,
    session_id: &str,
    raw: &str,
    parsed: &TranscriptSummary,
    request: &TranscriptRequest<'_>,
) -> Result<(TranscriptOutcome, Keep)> {
    let context = session_context(session_id, parsed, request);

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
                (MESSAGE_IDS_KEY, whole_ids(raw, parsed).into()),
                (
                    ROLES_KEY,
                    roles_of(&messages_of(raw).iter().collect::<Vec<_>>()).into(),
                ),
            ],
        ),
        // The stamp carries the session's project.
        project: None,
    };
    let stamp = CaptureStamp {
        context,
        written_by: Some("hook".into()),
        capture_method: Some("auto".into()),
        authoritative: request.context.is_some(),
    };
    let made = auto_capture_stamped(store, &input, &stamp)?;
    let keep = Keep::First {
        row: made.dialog_id.clone(),
        messages: parsed.messages,
    };
    let outcome = TranscriptOutcome {
        capture_id: made.capture_id,
        dialog_id: made.dialog_id,
        summary_id: made.summary_id,
        messages: parsed.messages,
    };
    Ok((outcome, keep))
}

/// Whether `session_id` already has a capture.
pub fn is_captured(store: &Store<'_>, session_id: &str) -> Result<bool> {
    Ok(existing_dialog(store, session_id)?.is_some())
}

/// How many messages `session_id`'s capture holds, or `None` when it has
/// none.
pub fn captured_messages(store: &Store<'_>, session_id: &str) -> Result<Option<usize>> {
    existing_dialog(store, session_id)?
        .as_ref()
        .map(|dialog| held_by(store, dialog))
        .transpose()
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

/// Record a grown session: the turns no row holds yet go into a new,
/// read-only continuation dialog row, and the summary half is refreshed with
/// `parsed`, recording what it replaced. Both land in one transaction, so a
/// failure cannot leave turns recorded under a summary that does not count
/// them. The first dialog row is never rewritten: dialogs are the verbatim
/// record and read-only (D13, pack Q14, `crate::dialog`).
fn recapture(
    store: &Store<'_>,
    dialog: &crate::models::Memory,
    raw: &str,
    parsed: &TranscriptSummary,
    reason: Option<&str>,
) -> Result<(TranscriptOutcome, Keep)> {
    let (outcome, written, keep) =
        store.transaction(|page| recapture_in(page, dialog, raw, parsed, reason))?;
    // Embedded after the commit, best-effort as an update is: a model call
    // never runs inside the write, and its failure undoes nothing.
    if let Some(embedder) = crate::embedder::available_embedder() {
        for (id, content) in &written {
            let _ = crate::vectors::embed_and_store(store, &*embedder, id, content);
        }
    }
    Ok((outcome, keep))
}

/// Ids and stored text of the rows a recapture wrote, for embedding.
type Written = Vec<(String, String)>;

fn recapture_in(
    store: &Store<'_>,
    dialog: &crate::models::Memory,
    raw: &str,
    parsed: &TranscriptSummary,
    reason: Option<&str>,
) -> Result<(TranscriptOutcome, Written, Keep)> {
    let capture_id = dialog.capture_id.clone().ok_or(StoreError::NotFound)?;
    let rows = dialog_rows(store, dialog)?;
    let held: usize = rows.iter().map(held_messages).sum();

    let tools = MessageTools::from_env();
    let messages = messages_of(raw);
    let blocks = stored_blocks(&messages, &tools);
    let record_roles = record_roles(&rows);
    let roles: HashSet<&str> = record_roles
        .iter()
        .map(String::as_str)
        .chain(roles(&blocks))
        .collect();
    // An unplaced row sits outside the placed sequence: text placement
    // compares against placed rows only. Its ids still count as held.
    let placed_rows: Vec<&crate::models::Memory> =
        rows.iter().filter(|row| !is_unplaced(row)).collect();
    let mut recorded: Vec<Block<'_>> = Vec::new();
    let mut recorded_positions: Vec<Option<String>> = Vec::new();
    for row in &placed_rows {
        let row_split = row_blocks(row, &roles);
        let mut row_ids = position_ids(row);
        // Ids that do not line up one per message say nothing about any.
        if row_ids.len() != row_split.len() {
            row_ids = vec![None; row_split.len()];
        }
        recorded.extend(row_split);
        recorded_positions.extend(row_ids);
    }
    let block_ids: Vec<Option<&str>> = recorded_positions.iter().map(|id| id.as_deref()).collect();
    let placed_count: usize = placed_rows.iter().map(|row| held_messages(row)).sum();
    let held_ids = recorded_ids(&rows);
    let ids = message_ids(&messages);
    let position: Vec<Option<String>> = messages.iter().map(|m| m.uuid.clone()).collect();
    let incoming = Incoming {
        blocks: &blocks,
        ids: ids.as_deref(),
        position_ids: &position,
    };
    let placement = place(
        &Record {
            blocks: &recorded,
            ids: held_ids.as_ref(),
            block_ids: &block_ids,
        },
        &incoming,
    );
    // A submission an unplaced row already holds exactly, such as a retry
    // of the same tail, is not kept a second time.
    let placement = match placement {
        Placement::Unplaced(_) if held_unplaced(&rows, &roles, &blocks, &position) => {
            Placement::Held
        }
        other => other,
    };
    let mut written = Written::new();
    let indexes: Vec<usize> = match &placement {
        // An older or equal copy: nothing is added, and the summary keeps
        // describing the longer record rather than this copy.
        Placement::Held => {
            let outcome = held_outcome(store, dialog, &capture_id, held)?;
            let keep = Keep::Repair(held_rows(&rows, &roles, &blocks, &position));
            return Ok((outcome, written, keep));
        }
        Placement::New { indexes, .. } => indexes.clone(),
        // Not shown where it fits: kept whole and flagged, never skipped.
        Placement::Unplaced(_) => (0..messages.len()).collect(),
    };
    let new: Vec<&ChatMessage> = indexes.iter().map(|&i| &messages[i]).collect();
    let text = new
        .iter()
        .map(|m| render_message(m, &tools))
        .collect::<Vec<_>>()
        .join(MESSAGE_SEPARATOR);
    // The first row is 0, so the next continuation is the row count.
    let sequence = rows.len();
    let continuation = Continuation {
        roles: roles_of(&new),
        sequence,
        from: placed_count,
        count: new.len(),
        ids: new.iter().map(|m| m.uuid.clone()).collect(),
        placement: placement.label(),
        reason,
    };
    let (row, stored) = append_continuation(store, dialog, &text, &continuation)?;
    let keep = Keep::Continuation {
        row: row.clone(),
        placed: matches!(placement, Placement::New { .. }),
        indexes,
    };
    written.push((row, stored));
    let total = held + new.len();

    let capture = get_capture(store, &capture_id)?.ok_or(StoreError::NotFound)?;
    let summary = capture.summary.ok_or(StoreError::NotFound)?;
    let title = parsed.title();
    let pairs = [
        ("title", serde_json::json!(title)),
        ("messages", serde_json::json!(total)),
        ("reason", serde_json::json!(reason)),
    ];
    let content = replace_text(
        store,
        &summary,
        &parsed.summary(),
        &with_keys(&summary.metadata, &pairs),
    )?;
    written.push((summary.id.clone(), content));
    let outcome = TranscriptOutcome {
        capture_id,
        dialog_id: dialog.id.clone(),
        summary_id: summary.id,
        messages: total,
    };
    Ok((outcome, written, keep))
}

/// The rows of the record that `blocks` holds exactly, message for message,
/// with where in it they sit: what a held copy may repair a lost source
/// from. A row's messages are found by the ids it lists, each wherever it
/// sits (a continuation of interleaved turns is not contiguous), else from
/// its recorded start (the first row at 0, an unplaced row at 0, a placed
/// continuation at `from_message`). The ids must agree and the text must
/// equal the row's, message for message: matching ids alone do not prove
/// the bytes are the row's. A row it does not show is left out, never
/// guessed.
fn held_rows(
    rows: &[crate::models::Memory],
    roles: &HashSet<&str>,
    blocks: &[String],
    ids: &[Option<String>],
) -> Vec<Keep> {
    rows.iter()
        .enumerate()
        .filter_map(|(sequence, row)| {
            let at = row_positions(row, sequence, ids)?;
            let held: Vec<String> = at
                .iter()
                .map(|&i| blocks.get(i).cloned())
                .collect::<Option<_>>()?;
            let held_ids: Vec<Option<String>> = at
                .iter()
                .map(|&i| ids.get(i).cloned())
                .collect::<Option<_>>()?;
            let fits = !at.is_empty()
                && holds_exactly(row, roles, &held, &held_ids)
                && same_text(row, roles, &held);
            if !fits {
                return None;
            }
            let id = row.id.clone();
            Some(match sequence {
                0 => Keep::First {
                    row: id,
                    messages: at.len(),
                },
                _ => Keep::Continuation {
                    row: id,
                    indexes: at,
                    placed: false,
                },
            })
        })
        .collect()
}

/// Where the messages of `row`, the `sequence`th of its record, sit in a
/// transcript whose messages carry `ids`, in order.
fn row_positions(
    row: &crate::models::Memory,
    sequence: usize,
    ids: &[Option<String>],
) -> Option<Vec<usize>> {
    let count = held_messages(row);
    let listed = position_ids(row);
    if sequence > 0 && !listed.is_empty() && listed.iter().all(Option::is_some) {
        let at: Vec<usize> = listed
            .iter()
            .map(|want| ids.iter().position(|id| id == want))
            .collect::<Option<_>>()?;
        return at.windows(2).all(|w| w[0] < w[1]).then_some(at);
    }
    let start = match sequence {
        0 => 0,
        _ if is_unplaced(row) => 0,
        _ => row.metadata.get("from_message")?.as_u64()? as usize,
    };
    Some((start..start.checked_add(count)?).collect())
}

/// What a recapture of a copy the record already holds reports: the capture
/// as it stands, with nothing written.
fn held_outcome(
    store: &Store<'_>,
    dialog: &crate::models::Memory,
    capture_id: &str,
    held: usize,
) -> Result<TranscriptOutcome> {
    let capture = get_capture(store, capture_id)?.ok_or(StoreError::NotFound)?;
    let summary = capture.summary.ok_or(StoreError::NotFound)?;
    Ok(TranscriptOutcome {
        capture_id: capture_id.to_string(),
        dialog_id: dialog.id.clone(),
        summary_id: summary.id,
        messages: held,
    })
}

pub use crate::models::CONTINUATION_TYPE;

/// Where a continuation row sits in its session's record.
struct Continuation<'a> {
    /// The distinct roles of its messages.
    roles: Vec<String>,
    /// Its place among the session's dialog rows: the first row is 0, each
    /// continuation one more than the last. Rows are read back in this
    /// order, never by timestamp.
    sequence: usize,
    /// Messages recorded before it.
    from: usize,
    /// Messages it holds.
    count: usize,
    /// Each message's id, `None` where it had none: placement by id trusts
    /// the list only when every entry is known ([`listed_ids`]), and the
    /// retry check compares it position by position.
    ids: Vec<Option<String>>,
    /// How its messages were placed ([`Placement::label`]): `id` or
    /// `overlap`, or `unplaced_*` when the transcript was kept whole because
    /// no placement could be shown.
    placement: &'static str,
    reason: Option<&'a str>,
}

/// Insert a dialog row holding the session's turns after the record so far,
/// sharing `first`'s capture id, scrubbed as a first capture is. Not cut to
/// [`MAX_DIALOG_CHARS`]: the record keeps every turn, and a transcript's size
/// is already bounded where it arrives. Returns its id and stored text. Its
/// category is [`crate::models::DIALOG_CATEGORY`], so it is as read-only as
/// the first row.
fn append_continuation(
    store: &Store<'_>,
    first: &crate::models::Memory,
    text: &str,
    at: &Continuation<'_>,
) -> Result<(String, String)> {
    let now = Utc::now().to_rfc3339();
    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let mut tags = first.tags.clone();
    let mut metadata = serde_json::json!({
        "type": CONTINUATION_TYPE,
        "continues": first.id,
        crate::models::SEQUENCE_KEY: at.sequence,
        ROLES_KEY: at.roles,
        "from_message": at.from,
        "messages": at.count,
        "placement": at.placement,
        MESSAGE_IDS_KEY: at.ids,
        "reason": at.reason,
    });
    let content = crate::boundary::scrub(text, &mut tags, &mut metadata);
    // Stamped as the first capture is (node, client, writer), then given
    // the first row's session fields, so a continuation is attributed
    // where its record is.
    let provenance = crate::context::default_provenance().auto();
    let row = NewMemory {
        category: crate::models::DIALOG_CATEGORY.to_string(),
        tags,
        source: first.source.clone(),
        metadata,
        capture_id: first.capture_id.clone(),
        memory_type: first
            .memory_type
            .clone()
            .unwrap_or_else(|| "unclassified".into()),
        project: first.project.clone(),
        session_id: first.session_id.clone(),
        git_remote: first.git_remote.clone(),
        git_branch: first.git_branch.clone(),
        git_sha: first.git_sha.clone(),
        cwd: first.cwd.clone(),
        // A sensitive record stays sensitive in every row of it.
        sensitive: first.sensitive,
        written_by: "hook".into(),
        capture_method: "auto".into(),
        ..provenance.stamp(NewMemory::new(id.clone(), content.clone(), &now))
    };
    Memories::new(store).insert(&row)?;
    Ok((id, content))
}

/// Edit a memory's content and metadata, writing a `recapture` revision of
/// what it held, and return the stored text for embedding. The text is
/// scrubbed as a first capture's is, so a refresh never stores what a
/// capture would have redacted. A dialog is refused: only the summary half
/// is ever refreshed.
fn replace_text(
    store: &Store<'_>,
    memory: &crate::models::Memory,
    content: &str,
    metadata: &serde_json::Value,
) -> Result<String> {
    crate::dialog::refuse_dialog_edit(memory)?;
    let mut tags = memory.tags.clone();
    let mut metadata = metadata.clone();
    let content = crate::boundary::scrub(content, &mut tags, &mut metadata);
    let tracked = TrackedChanges {
        content: Some(content.clone()),
        tags_json: Some(serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into())),
        metadata_json: Some(serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".into())),
        ..TrackedChanges::default()
    };
    capture_revision(store, &memory.id, &tracked, Some("recapture"))?;
    Memories::new(store).apply_edit(
        &memory.id,
        &MemoryEdit {
            content: Some(content.clone()),
            tags: Some(tags),
            metadata: Some(metadata),
            ..MemoryEdit::at(Utc::now().to_rfc3339())
        },
    )?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn slices_keep_selected_lines_and_fail_closed_on_a_shared_line() {
        // Messages 0..4 on lines a, b, b, c: line b holds messages 1 and 2.
        let spans = [0..1, 2..3, 2..3, 4..5];
        assert_eq!(slices(&spans, &[0, 3]), Some(vec![0..1, 4..5]));
        let shared_line = Range { start: 2, end: 3 };
        assert_eq!(slices(&spans, &[1, 2]), Some(vec![shared_line]));
        // Message 1 alone would claim message 2's bytes too.
        assert_eq!(slices(&spans, &[1]), None);
        assert_eq!(slices(&spans, &[9]), None);
    }

    #[test]
    fn slices_stay_linear_over_many_short_lines() {
        // 200,000 one-byte lines, two messages on every tenth. Scanning all
        // spans per selected span would take ~10^10 steps; one pass is
        // instant even unoptimised.
        let mut spans = Vec::new();
        for line in 0..200_000usize {
            let span = line * 2..line * 2 + 1;
            if line % 10 == 0 {
                spans.push(span.clone());
            }
            spans.push(span);
        }
        let indexes: Vec<usize> = (0..spans.len()).step_by(2).collect();
        let started = std::time::Instant::now();
        let chosen = slices(&spans, &indexes);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // Every tenth line's pair is split across the selection: refused.
        assert_eq!(chosen, None);
        let all: Vec<usize> = (0..spans.len()).collect();
        let whole = slices(&spans, &all).unwrap();
        assert_eq!(whole.len(), 200_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

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
        assert!(p
            .dialog
            .contains("tool: Bash — cargo test -p remind_me_core"));
        let summary = p.summary();
        assert!(summary.starts_with("First ask: fix the flaky test please\nLast reply: done"));
        assert!(summary.ends_with("Messages: 3\nTools used: Bash x1"));
    }

    fn post_line(text: &str) -> String {
        serde_json::json!({
            "type": "assistant", "sessionId": "s1",
            "timestamp": "2026-10-04T10:00:09.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": "mcp__hearthbot__post_message",
                 "input": {"text": text}},
                {"type": "tool_use", "id": "t3", "name": "Bash",
                 "input": {"command": "z".repeat(400)}}
            ]}
        })
        .to_string()
    }

    #[test]
    fn a_posted_message_is_kept_in_full_and_is_the_last_reply() {
        let posted = format!("Dream-RSI is the best fit.\n\n{}", "Because ".repeat(100));
        let raw = [transcript(""), post_line(&posted)].join("\n");
        let p = parse_with(&raw, &MessageTools::defaults());
        assert!(
            p.dialog
                .contains(&format!("tool: mcp__hearthbot__post_message — {posted}")),
            "{}",
            p.dialog
        );
        // Other tools keep their short preview.
        assert!(
            p.dialog
                .contains(&format!("tool: Bash — {}\n", "z".repeat(200)))
                || p.dialog
                    .ends_with(&format!("tool: Bash — {}", "z".repeat(200)))
        );
        assert!(!p.dialog.contains(&"z".repeat(201)));
        assert_eq!(p.last_assistant.as_deref(), Some(posted.as_str()));
        assert!(p
            .summary()
            .contains("Last reply: Dream-RSI is the best fit.\n\nBecause"));
    }

    fn mixed_line(blocks: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant", "sessionId": "s1",
            "message": {"role": "assistant", "content": blocks}
        })
        .to_string()
    }

    fn post(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "tool_use", "id": format!("t-{text}"),
            "name": "mcp__hearthbot__post_message", "input": {"text": text}})
    }

    fn said(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "text", "text": text})
    }

    /// The reply and dialog of one assistant message made of `blocks`.
    fn read(blocks: serde_json::Value) -> (Option<String>, String) {
        let p = parse_with(&mixed_line(blocks), &MessageTools::defaults());
        (p.last_assistant, p.dialog)
    }

    #[test]
    fn the_last_reply_and_the_dialog_follow_block_order() {
        let (reply, dialog) = read(serde_json::json!([said("first"), post("posted")]));
        assert_eq!(reply.as_deref(), Some("posted"));
        assert_eq!(
            dialog,
            "**assistant:** first\ntool: mcp__hearthbot__post_message — posted"
        );

        let (reply, dialog) = read(serde_json::json!([
            post("Initial answer"),
            said("Correction")
        ]));
        assert_eq!(reply.as_deref(), Some("Correction"));
        assert_eq!(
            dialog,
            "**assistant:** tool: mcp__hearthbot__post_message — Initial answer\nCorrection"
        );

        let blocks = serde_json::json!([said("a"), post("b"), said("c"), post("d")]);
        assert_eq!(read(blocks).0.as_deref(), Some("d"));
        let blocks = serde_json::json!([post("a"), said("b"), post("c"), said("d")]);
        let (reply, dialog) = read(blocks);
        assert_eq!(reply.as_deref(), Some("d"));
        assert!(dialog.ends_with("— a\nb\ntool: mcp__hearthbot__post_message — c\nd"));
    }

    #[test]
    fn text_blocks_keep_their_inner_whitespace() {
        let code = serde_json::json!([said("```python\nif ready:"), said("    run()\n```")]);
        assert!(read(code).1.contains("if ready:\n    run()\n```"));
        let mixed = serde_json::json!([
            said("```python\nif ready:"),
            said("    run()\n```"),
            post("p")
        ]);
        let (reply, dialog) = read(mixed);
        assert!(
            dialog.contains("if ready:\n    run()\n```\ntool:"),
            "{dialog}"
        );
        assert_eq!(reply.as_deref(), Some("p"));
        let (reply, _) = read(serde_json::json!([post("p"), said("  later  ")]));
        assert_eq!(reply.as_deref(), Some("later"));
    }

    #[test]
    fn a_tool_left_out_of_the_setting_keeps_its_preview() {
        let posted = "p".repeat(400);
        let raw = [line("user", "s1", "hi"), post_line(&posted)].join("\n");
        let p = parse_with(&raw, &MessageTools::from_setting(Some("")).unwrap());
        assert!(!p.dialog.contains(&posted));
        assert_eq!(p.last_assistant, None);
        assert!(p.summary().contains("Last reply: (none)"));
    }

    #[test]
    fn the_message_tool_setting_is_explicit_and_validated() {
        assert_eq!(
            MessageTools::from_setting(None).unwrap(),
            MessageTools::defaults()
        );
        let set = MessageTools::from_setting(Some(" a__post=text , b=body ")).unwrap();
        assert_eq!(
            set,
            MessageTools(vec![
                MessageTool {
                    name: "a__post".into(),
                    arg: "text".into()
                },
                MessageTool {
                    name: "b".into(),
                    arg: "body".into()
                },
            ])
        );
        for bad in ["post_message", "=text", "a=", "a=b,,c=d", "a=b=c"] {
            assert!(MessageTools::from_setting(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_truncates_title_and_dialog_and_skips_meta() {
        let long = "x".repeat(300);
        let meta =
            serde_json::json!({"isMeta": true, "message": {"role": "user", "content": "injected"}});
        let p = parse(&format!("{}\n{}", line("user", "s", &long), meta));
        assert_eq!(p.messages, 1);
        assert_eq!(p.title().chars().count(), TITLE_CHARS);
        let big = "y".repeat(MAX_DIALOG_CHARS + 50);
        let p = parse(&line("user", "s", &big));
        assert!(p.dialog.starts_with(TRUNCATION_MARKER));
        assert_eq!(
            p.dialog.chars().count(),
            TRUNCATION_MARKER.chars().count() + MAX_DIALOG_CHARS
        );
        assert_eq!(parse("").messages, 0);
    }

    fn request(reason: &str) -> TranscriptRequest<'_> {
        TranscriptRequest {
            session_id: "s1",
            cwd: Some(Path::new("/work/quokka")),
            reason: Some(reason),
            context: None,
        }
    }

    #[test]
    fn one_capture_per_session_and_recapture_appends_a_continuation() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let first = capture_transcript(&store, &transcript(""), &request("stop")).unwrap();
        assert_eq!(first.messages, 3);
        let before = crate::db::queries::get_memory_by_id(&store, &first.dialog_id)
            .unwrap()
            .unwrap();

        let grown = transcript(&line("user", "s1", "and one more thing"));
        let second = capture_transcript(&store, &grown, &request("end")).unwrap();
        assert_eq!(second.capture_id, first.capture_id);
        assert_eq!(second.dialog_id, first.dialog_id);
        assert_eq!(second.summary_id, first.summary_id);
        assert_eq!(second.messages, 4);

        let memories = crate::episodes::session_memories(&store, "s1").unwrap();
        assert_eq!(memories.len(), 3, "the pair and one continuation");
        let dialog = memories.iter().find(|m| m.id == first.dialog_id).unwrap();
        assert_eq!(
            dialog.content, before.content,
            "the first dialog row is read-only"
        );
        assert_eq!(dialog.updated_at, before.updated_at);
        let more = memories
            .iter()
            .find(|m| m.metadata["type"] == CONTINUATION_TYPE)
            .unwrap();
        assert_eq!(more.category, crate::models::DIALOG_CATEGORY);
        assert_eq!(more.capture_id.as_deref(), Some(first.capture_id.as_str()));
        assert_eq!(
            more.metadata["continues"],
            serde_json::json!(first.dialog_id)
        );
        assert_eq!(more.metadata["from_message"], serde_json::json!(3));
        assert_eq!(more.metadata["messages"], serde_json::json!(1));
        // These test lines' tool call carries no uuid, so text places them.
        assert_eq!(more.metadata["placement"], serde_json::json!("overlap"));
        assert!(
            more.content.contains("and one more thing"),
            "{}",
            more.content
        );
        assert!(
            !more.content.contains(&before.content),
            "only the new turns"
        );
        assert_eq!(more.session_id, dialog.session_id);

        // The same transcript again adds nothing.
        capture_transcript(&store, &grown, &request("stop")).unwrap();
        assert_eq!(
            crate::episodes::session_memories(&store, "s1")
                .unwrap()
                .len(),
            3
        );
        assert_eq!(dialog.written_by, "hook");
        assert_eq!(dialog.capture_method, "auto");
        assert_eq!(dialog.session_id.as_deref(), Some("s1"));
        assert_eq!(dialog.cwd.as_deref(), Some("/work/quokka"));
        assert_eq!(dialog.project.as_deref(), Some("quokka"));
        assert_eq!(dialog.metadata["reason"], "stop", "never rewritten");
        assert_eq!(more.metadata["reason"], "end");
        let summary = memories.iter().find(|m| m.id == first.summary_id).unwrap();
        assert!(summary.content.contains("Messages: 4"));

        let revisions = |id: &str| {
            crate::db::history::Revisions::new(&store)
                .list(id, 10)
                .unwrap()
        };
        assert!(revisions(&first.dialog_id).is_empty());
        let refreshed = revisions(&first.summary_id);
        assert!(!refreshed.is_empty());
        assert!(refreshed
            .iter()
            .all(|r| r.revision_reason.as_deref() == Some("recapture")));
        assert!(crate::db::sessions::Sessions::new(&store)
            .get("s1")
            .unwrap()
            .is_some());
    }

    #[test]
    fn sessions_do_not_share_a_capture_and_bad_input_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let a = capture_transcript(&store, &transcript(""), &request("stop")).unwrap();
        let other = TranscriptRequest {
            session_id: "s2",
            ..request("stop")
        };
        let b = capture_transcript(&store, &transcript(""), &other).unwrap();
        assert_ne!(a.capture_id, b.capture_id);

        assert!(capture_transcript(&store, "", &request("stop")).is_err());
        let blank = TranscriptRequest {
            session_id: " ",
            ..request("stop")
        };
        assert!(capture_transcript(&store, &transcript(""), &blank).is_err());
    }

    #[test]
    fn a_callers_context_is_stamped_instead_of_probing_its_cwd() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let given = WriteContext {
            project: Some("rusty_mill".into()),
            git_branch: Some("main".into()),
            git_sha: Some("abc123".into()),
            cwd: Some("/home/user/rusty_mill".into()),
            session_id: Some("ignored".into()),
            ..WriteContext::default()
        };
        let remote = TranscriptRequest {
            cwd: None,
            context: Some(&given),
            ..request("stop")
        };
        let made = capture_transcript(&store, &transcript(""), &remote).unwrap();
        let memories = crate::episodes::session_memories(&store, "s1").unwrap();
        let dialog = memories.iter().find(|m| m.id == made.dialog_id).unwrap();
        assert_eq!(dialog.session_id.as_deref(), Some("s1"));
        assert_eq!(dialog.project.as_deref(), Some("rusty_mill"));
        assert_eq!(dialog.git_branch.as_deref(), Some("main"));
        assert_eq!(dialog.git_sha.as_deref(), Some("abc123"));
        assert_eq!(dialog.cwd.as_deref(), Some("/home/user/rusty_mill"));
        // Not given, so unknown: never the remote of wherever the store runs.
        assert_eq!(dialog.git_remote, None);
    }

    #[test]
    fn a_recapture_redacts_as_a_first_capture_does() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let token = format!("ghp_{}", "a".repeat(36));
        capture_transcript(&store, &transcript(""), &request("stop")).unwrap();

        let leaked = line("user", "s1", &format!("use {token} to push"));
        let made = capture_transcript(&store, &transcript(&leaked), &request("stop")).unwrap();

        let memories = crate::episodes::session_memories(&store, "s1").unwrap();
        assert!(memories.iter().all(|m| !m.content.contains(&token)));
        let more = memories
            .iter()
            .find(|m| {
                m.metadata["type"] == CONTINUATION_TYPE
                    && m.capture_id.as_deref() == Some(made.capture_id.as_str())
            })
            .unwrap();
        assert!(
            more.content.contains("[REDACTED:github_token]"),
            "{}",
            more.content
        );
        assert!(more.tags.iter().any(|t| t == crate::redact::REDACTED_TAG));
        assert_eq!(
            more.metadata["redactions"],
            serde_json::json!(["github_token"])
        );
    }

    #[test]
    fn a_secret_in_the_first_prompt_reaches_no_title_on_either_path() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let token = format!("ghp_{}", "d".repeat(36));
        let clean = |title: &str| !title.contains("ghp_") && !title.contains("dddddddd");
        let titles = |store: &Store<'_>| -> Vec<String> {
            crate::episodes::session_memories(store, "s1")
                .unwrap()
                .iter()
                .map(|m| m.metadata["title"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        // The token starts before the title's cut and ends after it.
        let ask = format!("{} {token} please", "x".repeat(TITLE_CHARS - 10));
        let leaky = [line("user", "s1", &ask), line("assistant", "s1", "ok")].join("\n");
        assert!(clean(&parse(&leaky).title()), "{}", parse(&leaky).title());

        // First capture.
        capture_transcript(&store, &leaky, &request("stop")).unwrap();
        let first = titles(&store);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|t| clean(t) && !t.is_empty()), "{first:?}");

        // Recapture over a session first captured clean: only the summary
        // is refreshed, so only its title changes.
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let made = capture_transcript(&store, &transcript(""), &request("stop")).unwrap();
        capture_transcript(&store, &leaky, &request("stop")).unwrap();
        let again = titles(&store);
        // The leaky transcript shares no turn with the record, so it is kept
        // whole in a continuation row, which carries no title.
        assert_eq!(again.len(), 3);
        assert!(again.iter().all(|t| clean(t)), "{again:?}");
        let summary = crate::db::queries::get_memory_by_id(&store, &made.summary_id)
            .unwrap()
            .unwrap();
        assert!(summary.metadata["title"]
            .as_str()
            .unwrap()
            .starts_with("xxx"));
    }

    #[test]
    fn encoded_len_is_what_json_escaping_costs() {
        let text = "plain \"quoted\" back\\slash\nline\ttab\u{1}ctl é";
        let escaped = serde_json::to_string(text).unwrap();
        assert_eq!(encoded_len(text), escaped.len() - 2);
        assert_eq!(encoded_len(&"\"".repeat(10)), 20);
    }
}
