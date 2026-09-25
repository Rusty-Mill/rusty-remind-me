//! File import: chat exports and documents into memories.
//!
//! This is how a vault gets populated with anything beyond hand-written
//! memories. It also brings two shipped-but-inert features to life:
//! `normalize_batch` selects `source IN ('document_import', 'chat_import')`,
//! and `include_neighbors` keys on `doc_id`/`chunk_index` — none of which
//! anything wrote before this.
//!
//! # Chat versus document
//!
//! `.json`/`.jsonl` are always chat exports. `.md`/`.markdown`/`.txt` are
//! **content-sniffed** in `auto` mode: text carrying chat role markers
//! (`## Human`, `**Assistant:**`) parses as chat, everything else as a
//! document. A document is split per Markdown section with its heading
//! breadcrumb carried into the chunk; plain text is paragraph-chunked.
//!
//! # Dedup
//!
//! Keyed on a hash of the file's raw bytes, recorded in `chat_imports`, so
//! re-importing the same content is a no-op regardless of filename. The hash
//! is checked **before** the file's text is read or parsed, so a re-import
//! short-circuits without doing the work.

use crate::entity::{upsert_entity, upsert_entity_relation};
use crate::import_paths::{
    suffix_of, validate_import_dir, validate_import_file, AUDIO_SUFFIXES, DOCUMENT_SUFFIXES,
    IMAGE_SUFFIXES, SUPPORTED_SUFFIXES,
};
use crate::models::{
    BulkImportDirInput, BulkImportResult, ChatImportInput, EntityInput, ImportKind, ImportOutcome,
    ImportStats, IMPORT_MAX_LENGTH_MAX, IMPORT_MAX_LENGTH_MIN,
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};

/// `source` assigned to memories from a chat export.
pub const CHAT_SOURCE: &str = "chat_import";
/// `source` assigned to memories from a document.
pub const DOCUMENT_SOURCE: &str = "document_import";
/// `category` a document import falls back to when the caller left the
/// chat-shaped default in place.
pub const DOCUMENT_CATEGORY: &str = "document";

/// What parsing produced: chunk/section pairs, the pre-chunk entry count, the
/// `source` to store under, and the resolved category.
///
/// Named rather than a tuple because the two counts and the two strings are
/// easy to transpose.
struct ParsedImport {
    chunks: Vec<(String, Option<String>)>,
    /// Per-chunk byte spans into the raw file, positionally aligned with
    /// `chunks`. Empty for every kind but a chat export, and `None` within it
    /// for the formats whose chunks do not trace to one contiguous region.
    /// Consumed only by raw-transcript retention (#212); an empty vector just
    /// means no memory records a source span.
    spans: Vec<Option<(usize, usize)>>,
    raw_entries: usize,
    source: &'static str,
    category: String,
    /// Per-chunk extras, positionally aligned with `chunks`. Empty for every
    /// kind but Obsidian, which is the only one that attaches tags and entity
    /// mentions the caller did not supply.
    extras: Vec<ChunkExtras>,
    /// Note-level frontmatter, merged into every chunk's metadata.
    frontmatter: serde_json::Map<String, serde_json::Value>,
}

/// What an Obsidian note contributes to a chunk beyond its text.
#[derive(Debug, Clone, Default)]
struct ChunkExtras {
    extra_tags: Vec<String>,
    mention_entities: Vec<String>,
    /// Connector-supplied metadata merged into the chunk's own.
    metadata: serde_json::Map<String, serde_json::Value>,
}

/// Short content fingerprint used for dedup.
fn hash_bytes(data: &[u8]) -> String {
    sha256::digest(data)[..16].to_string()
}

/// Split text at natural boundaries, preferring the largest that fits.
///
/// Paragraph, then line, then sentence, then a hard cut. A window that is
/// nothing but whitespace strips to empty and is dropped rather than stored —
/// otherwise a long run of indentation becomes a blank memory.
pub fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }

    let mut chunks = Vec::new();
    let mut rest = text.to_string();
    while !rest.is_empty() {
        if rest.chars().count() <= max_len {
            if !rest.trim().is_empty() {
                chunks.push(rest);
            }
            break;
        }
        // Byte offset of the character boundary `max_len` characters in, so
        // every slice below lands on a character boundary.
        let window_end = rest
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..window_end];

        let cut = window
            .rfind("\n\n")
            .or_else(|| window.rfind('\n'))
            .or_else(|| window.rfind(". "))
            .map(|i| i + 1)
            .unwrap_or(window_end);

        let chunk = rest[..cut].trim().to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        rest = rest[cut..].trim_start().to_string();
    }
    chunks
}

/// One `{role, content}` message pulled out of a chat export.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Flatten a message `content` field to plain text.
///
/// A content field is either a bare string or a list of blocks. Text blocks
/// contribute their text; `tool_use`, `tool_result`, `thinking` and image
/// blocks contribute nothing — they are machine chatter rather than
/// conversation, and storing them would bury recallable facts under transcript
/// noise.
///
/// A block with **no `type` at all but a `text` key** is kept: older exports
/// omit the discriminator, and dropping those would silently lose real
/// conversation to a format detail.
///
/// Empty parts are filtered before joining, so a message that is half tool
/// calls does not arrive padded with blank lines.
fn text_of(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => {
                    let kind = o.get("type").and_then(|t| t.as_str());
                    let keep = kind == Some("text") || (kind.is_none() && o.contains_key("text"));
                    keep.then(|| {
                        o.get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string()
                    })
                }
                _ => None,
            })
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string().trim().to_string(),
    }
}

fn push_message(out: &mut Vec<ChatMessage>, role: &str, content: String) {
    if !content.trim().is_empty() {
        out.push(ChatMessage {
            role: role.to_string(),
            content: content.trim().to_string(),
        });
    }
}

/// Pull messages out of whatever JSON shape the export uses.
///
/// Handles a bare `{role, content}`, a list of them, a `{messages: [...]}`
/// wrapper, Claude's `chat_messages` with block content, Claude Code session
/// transcripts (one `{"type": …, "message": {…}}` envelope per JSONL line),
/// and a list of conversations containing either.
///
/// Records carrying a `record_type` are **entity-graph records from an
/// export**, not messages, and are skipped here — they are restored
/// separately by [`restore_graph_records`].
pub fn extract_messages(data: &serde_json::Value) -> Vec<ChatMessage> {
    let mut messages = Vec::new();

    if let Some(object) = data.as_object() {
        if object.contains_key("record_type") {
            return messages;
        }
        if let Some(chat_messages) = object.get("chat_messages").and_then(|v| v.as_array()) {
            for message in chat_messages {
                let role = message
                    .get("sender")
                    .or_else(|| message.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let content = message
                    .get("content")
                    .or_else(|| message.get("text"))
                    .map(text_of)
                    .unwrap_or_default();
                push_message(&mut messages, role, content);
            }
            return messages;
        }
        // Claude Code session transcript: one envelope per JSONL line, with
        // the real message under `message`. Without this branch the envelope
        // matches nothing below and the whole transcript imports as zero
        // memories — recorded as a success, so the only symptom is a count of
        // 0 that looks exactly like an empty file.
        //
        // Placed after `chat_messages` and before the bare `{role, content}`
        // branch, and guarded on the inner object actually carrying a role, so
        // any other shape that happens to have a `message` key still falls
        // through unchanged.
        if let Some(inner) = object.get("message").and_then(|m| m.as_object()) {
            if inner.contains_key("role") || inner.contains_key("sender") {
                let role = inner
                    .get("role")
                    .or_else(|| inner.get("sender"))
                    .and_then(|v| v.as_str())
                    // The envelope's own `type` names the speaker when the
                    // inner message omits the role.
                    .or_else(|| object.get("type").and_then(|v| v.as_str()))
                    .unwrap_or("unknown");
                let content = inner
                    .get("content")
                    .or_else(|| inner.get("text"))
                    .map(text_of)
                    .unwrap_or_default();
                push_message(&mut messages, role, content);
                return messages;
            }
        }
        if let Some(inner) = object.get("messages") {
            return extract_messages(inner);
        }
        if object.contains_key("role") || object.contains_key("sender") {
            return extract_messages(&serde_json::Value::Array(vec![data.clone()]));
        }
    }

    if let Some(items) = data.as_array() {
        for item in items {
            let Some(object) = item.as_object() else {
                continue;
            };
            if object.contains_key("record_type") {
                continue;
            }
            if object.contains_key("messages") || object.contains_key("chat_messages") {
                messages.extend(extract_messages(item));
            } else if object.contains_key("role") || object.contains_key("sender") {
                let role = object
                    .get("role")
                    .or_else(|| object.get("sender"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let content = object
                    .get("content")
                    .or_else(|| object.get("text"))
                    .map(text_of)
                    .unwrap_or_default();
                push_message(&mut messages, role, content);
            }
        }
    }

    messages
}

/// Reduce messages to the content strings a mode asks for.
pub fn filter_messages(messages: &[ChatMessage], mode: &str) -> Vec<String> {
    match mode {
        "assistant_messages" => messages
            .iter()
            .filter(|m| matches!(m.role.as_str(), "assistant" | "bot"))
            .map(|m| m.content.clone())
            .collect(),
        "user_messages" => messages
            .iter()
            .filter(|m| matches!(m.role.as_str(), "user" | "human"))
            .map(|m| m.content.clone())
            .collect(),
        "all_messages" => messages
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect(),
        // The whole exchange as one memory, rather than one per turn.
        "conversations" => {
            if messages.is_empty() {
                Vec::new()
            } else {
                vec![messages
                    .iter()
                    .map(|m| format!("**{}:** {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n\n")]
            }
        }
        "summaries" => messages
            .iter()
            .filter(|m| m.role.to_lowercase().contains("summary"))
            .map(|m| m.content.clone())
            .collect(),
        _ => messages.iter().map(|m| m.content.clone()).collect(),
    }
}

/// Does this line open or close a fenced code block?
fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Parse an ATX heading into (level, title).
fn heading_of(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_end();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed[hashes..].trim_start();
    if rest.is_empty() || !trimmed[hashes..].starts_with(char::is_whitespace) {
        return None;
    }
    Some((hashes, rest.trim_end_matches('#').trim().to_string()))
}

/// Split chat role markers out of markdown text.
///
/// Recognises `## Human`, `**Assistant:**`, `User:` and friends. An empty
/// result is what `auto` mode uses to decide a file is a document rather than
/// a chat log, so this doubles as the sniffer.
pub fn split_chat_markdown(text: &str) -> Vec<ChatMessage> {
    const ROLES: [&str; 6] = ["human", "user", "assistant", "claude", "bot", "system"];

    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    for line in text.lines() {
        let bare = line
            .trim()
            .trim_start_matches('#')
            .trim()
            .trim_start_matches("**")
            .trim_end_matches("**")
            .trim_end_matches(':')
            .trim_end_matches("**")
            .trim();
        let matched = ROLES
            .iter()
            .find(|role| bare.eq_ignore_ascii_case(role))
            .map(|role| role.to_string());

        match matched {
            Some(role) => {
                if let Some((previous, lines)) = current.take() {
                    push_message(&mut messages, &previous, lines.join("\n"));
                }
                current = Some((role, Vec::new()));
            }
            None => {
                if let Some((_, lines)) = current.as_mut() {
                    lines.push(line.to_string());
                }
            }
        }
    }
    if let Some((role, lines)) = current {
        push_message(&mut messages, &role, lines.join("\n"));
    }
    messages
}

/// Whether markdown text carries chat structure.
pub fn looks_like_chat_markdown(text: &str) -> bool {
    !split_chat_markdown(text).is_empty()
}

/// Split markdown into `(heading breadcrumb, body)` sections.
///
/// The breadcrumb is the section's ancestor headings joined with ` > `, so
/// nested context travels with the chunk. Headings inside fenced code blocks
/// are not headings. Heading-only sections are dropped.
pub fn split_markdown_sections(text: &str) -> Vec<(Option<String>, String)> {
    let mut sections = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut heading: Option<String> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut in_fence = false;

    for line in text.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            lines.push(line.to_string());
            continue;
        }
        match if in_fence { None } else { heading_of(line) } {
            Some((level, title)) => {
                let body = lines.join("\n").trim().to_string();
                if !body.is_empty() {
                    sections.push((heading.clone(), body));
                }
                lines.clear();
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, title));
                heading = Some(
                    stack
                        .iter()
                        .map(|(_, t)| t.as_str())
                        .collect::<Vec<_>>()
                        .join(" > "),
                );
            }
            None => lines.push(line.to_string()),
        }
    }
    let body = lines.join("\n").trim().to_string();
    if !body.is_empty() {
        sections.push((heading, body));
    }
    sections
}

/// Chunk a document into `(content, section heading)` pairs.
///
/// Markdown keeps its heading breadcrumb both prepended to the content — so
/// search sees the context — and separate, for the memory's metadata. The
/// prefix comes out of the chunk budget, floored so a pathological heading
/// cannot reduce the body budget to nothing.
pub fn parse_document(
    text: &str,
    suffix: &str,
    max_length: usize,
) -> Vec<(String, Option<String>)> {
    let mut pairs = Vec::new();
    if matches!(suffix, "md" | "markdown") {
        for (heading, body) in split_markdown_sections(text) {
            let prefix = heading
                .as_ref()
                .map(|h| format!("{}\n\n", h))
                .unwrap_or_default();
            let budget = max_length.saturating_sub(prefix.chars().count()).max(100);
            for chunk in chunk_text(&body, budget) {
                pairs.push((format!("{}{}", prefix, chunk), heading.clone()));
            }
        }
    } else {
        for chunk in chunk_text(text, max_length) {
            pairs.push((chunk, None));
        }
    }
    pairs
}

/// A parsed unit of a chat export, and where in the raw file it came from.
///
/// `span` is a byte range into the `raw` that produced it, present only for
/// formats where one unit traces to one contiguous region. JSONL does: one
/// envelope per line. A whole-file JSON array does not — every message shares
/// the one top-level value — and markdown role-splitting is a text transform
/// whose output no longer indexes its input. Those get `None` rather than a
/// guessed range, because a wrong span is worse than a missing one: it would
/// hand a caller some other turn's bytes and look authoritative doing it.
struct ChatChunk {
    content: String,
    span: Option<(usize, usize)>,
}

/// Parse a chat export into chunked content strings.
fn parse_chat(
    raw: &str,
    suffix: &str,
    extract_mode: &str,
    max_length: usize,
) -> (Vec<ChatChunk>, usize) {
    // (content, originating byte span) before chunking.
    let mut contents: Vec<(String, Option<(usize, usize)>)> = Vec::new();

    match suffix {
        "json" => {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(raw) {
                let conversations = data.as_array().is_some_and(|items| {
                    items.first().and_then(|f| f.as_object()).is_some_and(|o| {
                        o.contains_key("chat_messages") || o.contains_key("messages")
                    })
                });
                if conversations {
                    for conversation in data.as_array().unwrap() {
                        let messages = extract_messages(conversation);
                        contents.extend(
                            filter_messages(&messages, extract_mode)
                                .into_iter()
                                .map(|c| (c, None)),
                        );
                    }
                } else {
                    contents.extend(
                        filter_messages(&extract_messages(&data), extract_mode)
                            .into_iter()
                            .map(|c| (c, None)),
                    );
                }
            }
        }
        "jsonl" => {
            // Offsets are tracked against `raw` directly rather than via
            // `lines()`, which discards the separator and so cannot say where
            // a line began.
            let mut offset = 0usize;
            for line in raw.split_inclusive('\n') {
                let start = offset;
                offset += line.len();
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // A malformed line is skipped rather than failing the import —
                // one bad line in a long export should not lose the rest.
                let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                    continue;
                };
                if value.get("record_type").is_some() {
                    continue;
                }
                // The span covers the whole line, including the blocks
                // `text_of` dropped — recovering those is the entire point.
                let span = Some((start, offset));
                contents.extend(
                    filter_messages(&extract_messages(&value), extract_mode)
                        .into_iter()
                        .map(|c| (c, span)),
                );
            }
        }
        _ => {
            let messages = split_chat_markdown(raw);
            contents = if messages.is_empty() {
                // No structure found: the whole file is one memory — the one
                // markdown case where the span *is* knowable.
                if raw.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![(raw.trim().to_string(), Some((0, raw.len())))]
                }
            } else {
                filter_messages(&messages, extract_mode)
                    .into_iter()
                    .map(|c| (c, None))
                    .collect()
            };
        }
    }

    // `raw_entries` counts messages before chunking, which is what a caller
    // wants to know about a chat export; for documents chunking *is* the
    // extraction unit, so the two counts coincide there.
    let raw_entries = contents.len();
    let chunks = contents
        .iter()
        .filter(|(c, _)| !c.trim().is_empty())
        // Every chunk of a message inherits that message's span: they all came
        // from the same envelope, and the drill-down unit is the envelope.
        .flat_map(|(c, span)| {
            chunk_text(c, max_length)
                .into_iter()
                .map(move |content| ChatChunk {
                    content,
                    span: *span,
                })
        })
        .collect();
    (chunks, raw_entries)
}

/// Entity-graph records carried by an export.
fn extract_graph_records(raw: &str, suffix: &str) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    match suffix {
        "json" => {
            if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(raw) {
                records.extend(items.into_iter().filter(|i| i.get("record_type").is_some()));
            }
        }
        "jsonl" => {
            for line in raw.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    if value.get("record_type").is_some() {
                        records.push(value);
                    }
                }
            }
        }
        // A document never carries graph records.
        _ => {}
    }
    records
}

/// Restore exported entity-graph records.
///
/// Entities first, so a link's endpoint check sees rows restored moments
/// earlier. Entities upsert with the usual alias union-merge and re-derive
/// their deterministic id from the name.
///
/// **Links only restore when both endpoints exist.** A link references the
/// *original* memory id, and a chat re-import assigns new ones, so a link is
/// restorable only into a database that still holds the referenced memory.
/// Dangling links are skipped and counted rather than silently dropped or
/// forced — a restore that quietly invented endpoints would be worse than one
/// that reports what it could not do.
pub fn restore_graph_records(
    conn: &Connection,
    records: &[serde_json::Value],
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();

    for record in records {
        if record.get("record_type").and_then(|v| v.as_str()) != Some("entity") {
            continue;
        }
        let Some(name) = record.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let aliases = match record.get("aliases") {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect(),
            // Exported as a JSON string in some shapes.
            Some(serde_json::Value::String(raw)) => {
                serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        upsert_entity(
            conn,
            &EntityInput {
                name: name.to_string(),
                kind: record
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .filter(|k| !k.is_empty())
                    .map(str::to_string),
                aliases,
            },
        )?;
        stats.entities_restored += 1;
    }

    let exists = |table: &str, column: &str, id: &str| -> Result<bool> {
        let found: Option<i64> = conn
            .query_row(
                &format!("SELECT 1 FROM {} WHERE {} = ?", table, column),
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    };

    for record in records {
        match record.get("record_type").and_then(|v| v.as_str()) {
            Some("memory_entity") => {
                let (Some(memory_id), Some(entity_id)) = (
                    record.get("memory_id").and_then(|v| v.as_str()),
                    record.get("entity_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if !exists("memories", "id", memory_id)? || !exists("entities", "id", entity_id)? {
                    stats.links_skipped_dangling += 1;
                    continue;
                }
                if crate::entity::link_memory_entity(conn, memory_id, entity_id)? {
                    stats.links_restored += 1;
                }
            }
            Some("entity_relation") => {
                let (Some(subject), Some(relation), Some(object)) = (
                    record.get("subject_entity_id").and_then(|v| v.as_str()),
                    record.get("relation").and_then(|v| v.as_str()),
                    record.get("object_entity_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if !exists("entities", "id", subject)? || !exists("entities", "id", object)? {
                    stats.relations_skipped_dangling += 1;
                    continue;
                }
                if upsert_entity_relation(conn, subject, relation, object)? {
                    stats.relations_restored += 1;
                }
            }
            _ => {}
        }
    }

    Ok(stats)
}

/// Import already-read content.
///
/// Shared by the file and directory entry points. `raw` has been read,
/// `hash` computed, and the destination validated by the caller.
#[allow(clippy::too_many_arguments)]
pub fn import_content(
    conn: &Connection,
    raw: &str,
    // The file's original bytes. A text connector reads `raw`; a binary one
    // (PDF) must have these, because `raw` is a lossy UTF-8 decode that has
    // already destroyed the file.
    raw_bytes: &[u8],
    suffix: &str,
    filename: &str,
    hash: &str,
    category: &str,
    tags: &[String],
    extract_mode: &str,
    max_length: usize,
    kind: ImportKind,
) -> Result<ImportOutcome> {
    // Another caller may have imported the same content since the caller's
    // early check.
    if let Some(import_id) = existing_import(conn, hash)? {
        return Ok(ImportOutcome::Skipped {
            reason: "already_imported".to_string(),
            file: filename.to_string(),
            import_id,
        });
    }

    // `.json`/`.jsonl` are always chat exports; markdown and text are sniffed
    // in auto mode, so chat-shaped markdown keeps importing as chat.
    let effective = match (suffix, kind) {
        // An explicit Readwise request wins over the "json is always chat"
        // rule; `Auto` never reaches it, which is the whole point.
        (_, ImportKind::Readwise) => ImportKind::Readwise,
        // `.pdf` is unambiguous, so unlike Readwise it is safe to route from
        // `Auto` — there is no other thing a PDF could plausibly be.
        ("pdf", _) => ImportKind::Pdf,
        // Likewise for images and recordings: the suffix decides the parser
        // outright, because there is no text reading of these bytes at all.
        (s, _) if IMAGE_SUFFIXES.contains(&s) => ImportKind::Image,
        (s, _) if AUDIO_SUFFIXES.contains(&s) => ImportKind::Audio,
        ("json" | "jsonl", _) => ImportKind::Chat,
        (_, ImportKind::Auto) => {
            if looks_like_chat_markdown(raw) {
                ImportKind::Chat
            } else {
                ImportKind::Document
            }
        }
        (_, explicit) => explicit,
    };

    let graph_records = extract_graph_records(raw, suffix);

    let ParsedImport {
        chunks,
        raw_entries,
        source,
        category,
        extras,
        spans,
        frontmatter,
    } = match effective {
        ImportKind::Pdf => {
            let (pages, pages_with_text) = match crate::pdf_import::parse_pdf(raw_bytes, max_length)
            {
                Ok(parsed) => parsed,
                // Refused rather than imported as zero memories: a scan
                // that silently produces nothing is indistinguishable from
                // an empty file, which is the failure #147 fixed.
                Err(reason) => {
                    return Ok(ImportOutcome::Failed {
                        file: filename.to_string(),
                        reason,
                    })
                }
            };
            let mut chunks = Vec::with_capacity(pages.len());
            let mut extras = Vec::with_capacity(pages.len());
            for page in pages {
                chunks.push((page.content, None));
                let mut metadata = serde_json::Map::new();
                metadata.insert("page".to_string(), serde_json::json!(page.page));
                extras.push(ChunkExtras {
                    extra_tags: Vec::new(),
                    mention_entities: Vec::new(),
                    metadata,
                });
            }
            ParsedImport {
                chunks,
                // Pages that carried text, not chunks produced — the honest
                // answer to "how much of this document was readable".
                raw_entries: pages_with_text,
                extras,
                spans: Vec::new(),
                frontmatter: serde_json::Map::new(),
                source: crate::pdf_import::PDF_SOURCE,
                category: if category.is_empty() || category == CHAT_SOURCE {
                    crate::pdf_import::PDF_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        ImportKind::Image => {
            let (chunks, lines) = match crate::image_import::parse_image(raw_bytes, max_length) {
                Ok(parsed) => parsed,
                // An image with no readable text is refused rather than
                // imported as zero memories — otherwise it is indistinguishable
                // from an empty file, the failure #147 fixed.
                Err(reason) => {
                    return Ok(ImportOutcome::Failed {
                        file: filename.to_string(),
                        reason,
                    })
                }
            };
            let extras = vec![ChunkExtras::default(); chunks.len()];
            ParsedImport {
                chunks: chunks.into_iter().map(|c| (c, None)).collect(),
                // Recognised lines, not chunks produced — the honest answer to
                // "how much of this image was readable".
                raw_entries: lines,
                extras,
                spans: Vec::new(),
                frontmatter: serde_json::Map::new(),
                source: crate::image_import::IMAGE_SOURCE,
                category: if category.is_empty() || category == CHAT_SOURCE {
                    crate::image_import::IMAGE_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        ImportKind::Audio => {
            let (segments, segment_count) =
                match crate::audio_import::parse_audio(raw_bytes, max_length) {
                    Ok(parsed) => parsed,
                    // Silence is refused for the same reason a textless image
                    // is: a successful import of nothing hides the problem.
                    Err(reason) => {
                        return Ok(ImportOutcome::Failed {
                            file: filename.to_string(),
                            reason,
                        })
                    }
                };
            let mut chunks = Vec::with_capacity(segments.len());
            let mut extras = Vec::with_capacity(segments.len());
            for segment in segments {
                chunks.push((segment.content, None));
                let mut metadata = serde_json::Map::new();
                // Rounded to centiseconds, which is whisper.cpp's own
                // resolution — more digits would be invented precision.
                metadata.insert(
                    "start".to_string(),
                    serde_json::json!((segment.start * 100.0).round() / 100.0),
                );
                metadata.insert(
                    "end".to_string(),
                    serde_json::json!((segment.end * 100.0).round() / 100.0),
                );
                extras.push(ChunkExtras {
                    extra_tags: Vec::new(),
                    mention_entities: Vec::new(),
                    metadata,
                });
            }
            ParsedImport {
                chunks,
                // Segments transcribed, not chunks produced.
                raw_entries: segment_count,
                extras,
                spans: Vec::new(),
                frontmatter: serde_json::Map::new(),
                source: crate::audio_import::AUDIO_SOURCE,
                category: if category.is_empty() || category == CHAT_SOURCE {
                    crate::audio_import::AUDIO_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        ImportKind::Readwise => {
            let (highlights, raw_entries) =
                match crate::readwise_import::parse_export(raw, max_length) {
                    Ok(parsed) => parsed,
                    // A file that is not a Readwise export at all is refused
                    // outright rather than imported as zero memories — the
                    // silent-success failure mode #147 fixed elsewhere.
                    Err(reason) => {
                        return Ok(ImportOutcome::Failed {
                            file: filename.to_string(),
                            reason,
                        })
                    }
                };
            let mut chunks = Vec::with_capacity(highlights.len());
            let mut extras = Vec::with_capacity(highlights.len());
            for highlight in highlights {
                chunks.push((highlight.content, None));
                extras.push(ChunkExtras {
                    extra_tags: Vec::new(),
                    mention_entities: Vec::new(),
                    metadata: highlight.metadata,
                });
            }
            ParsedImport {
                chunks,
                raw_entries,
                extras,
                spans: Vec::new(),
                frontmatter: serde_json::Map::new(),
                source: crate::readwise_import::READWISE_SOURCE,
                category: if category.is_empty() || category == CHAT_SOURCE {
                    crate::readwise_import::READWISE_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        ImportKind::Obsidian => {
            let (notes, frontmatter) = crate::obsidian_import::parse_note(raw, max_length);
            let raw_entries = notes.len();
            let mut chunks = Vec::with_capacity(notes.len());
            let mut extras = Vec::with_capacity(notes.len());
            for note in notes {
                chunks.push((note.content, note.section));
                extras.push(ChunkExtras {
                    extra_tags: note.extra_tags,
                    mention_entities: note.mention_entities,
                    metadata: serde_json::Map::new(),
                });
            }
            ParsedImport {
                chunks,
                raw_entries,
                extras,
                spans: Vec::new(),
                frontmatter,
                source: crate::obsidian_import::OBSIDIAN_SOURCE,
                // Same rule as a document: the chat-shaped default gets
                // replaced, an explicit category is respected.
                category: if category.is_empty() || category == CHAT_SOURCE {
                    crate::obsidian_import::OBSIDIAN_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        ImportKind::Document => {
            let chunks = parse_document(raw, suffix, max_length);
            let raw_entries = chunks.len();
            ParsedImport {
                chunks,
                raw_entries,
                extras: Vec::new(),
                spans: Vec::new(),
                frontmatter: serde_json::Map::new(),
                source: DOCUMENT_SOURCE,
                // A document left under the chat-shaped default category gets
                // the document one instead; an explicit category is respected.
                category: if category.is_empty() || category == CHAT_SOURCE {
                    DOCUMENT_CATEGORY.to_string()
                } else {
                    category.to_string()
                },
            }
        }
        _ => {
            let (contents, raw_entries) = parse_chat(raw, suffix, extract_mode, max_length);
            let spans = contents.iter().map(|c| c.span).collect();
            ParsedImport {
                chunks: contents.into_iter().map(|c| (c.content, None)).collect(),
                raw_entries,
                extras: Vec::new(),
                spans,
                frontmatter: serde_json::Map::new(),
                source: CHAT_SOURCE,
                category: category.to_string(),
            }
        }
    };

    let now = Utc::now().to_rfc3339();
    let import_id = format!("imp_{}", uuid::Uuid::new_v4().simple());
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());

    // Raw-transcript retention (#212). A no-op unless REMIND_ME_ARCHIVE_DIR is
    // set. A failure here is swallowed on purpose: the memories below are the
    // import, and an unwritable archive directory must not turn a working
    // import into a failing one.
    let archived = crate::archive::store(conn, &import_id, filename, hash, raw_bytes)
        .unwrap_or(None)
        .is_some();

    // Spans index into `raw`; the blob holds `raw_bytes`. They agree only when
    // `raw` is that byte sequence rather than a lossy decode of it — true for
    // every text chat format, false for a PDF, whose spans are empty anyway.
    // Comparing outright rather than reasoning about which kinds are text
    // keeps a future binary connector from silently recording offsets into a
    // string that no longer indexes the file.
    let spans_addressable = archived && raw.as_bytes() == raw_bytes;

    // #266: every other write path that creates a `memories` row stamps
    // these (#258); this one didn't, so a chat import, directory import, or
    // webhook push always left `node_id` NULL and `client` at the schema
    // default 'unknown' regardless of what was actually configured. Hoisted
    // above the loop rather than called per chunk -- the identity cannot
    // change mid-import.
    let (node_id, client) = crate::sync::memory_provenance();

    let mut created = 0;
    for (chunk_index, (content, section)) in chunks.iter().enumerate() {
        let mut metadata = serde_json::json!({
            "import_id": import_id,
            "filename": filename,
        });
        if let Some(section) = section {
            metadata["section"] = serde_json::json!(section);
        }
        if !frontmatter.is_empty() {
            metadata["obsidian_frontmatter"] = serde_json::Value::Object(frontmatter.clone());
        }
        if let Some(extra) = extras.get(chunk_index) {
            for (key, value) in &extra.metadata {
                metadata[key] = value.clone();
            }
        }

        // A connector's tags are merged with the caller's rather than
        // replacing them: the caller asked for these memories to be tagged a
        // certain way, and the note's own tags are additional information, not
        // a correction. Deduplicated case-insensitively so `#Project` from the
        // note and `project` from the caller do not both land.
        let chunk_extras = extras.get(chunk_index);
        let chunk_tags_json = match chunk_extras {
            Some(e) if !e.extra_tags.is_empty() => {
                let merged = crate::obsidian_import::dedupe_ci(
                    tags.iter().cloned().chain(e.extra_tags.iter().cloned()),
                );
                serde_json::to_string(&merged).unwrap_or_else(|_| tags_json.clone())
            }
            _ => tags_json.clone(),
        };

        let memory_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

        // `doc_id`/`chunk_index` group every chunk of this file in source
        // order, which is what lets neighbour expansion find a hit's siblings
        // without re-parsing anything.
        conn.execute(
            "INSERT OR IGNORE INTO memories
                (id, content, category, tags, source, metadata, created_at, updated_at,
                 doc_id, chunk_index, node_id, client)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id,
                content,
                category,
                chunk_tags_json,
                source,
                metadata.to_string(),
                now,
                now,
                import_id,
                chunk_index as i64,
                node_id,
                client,
            ],
        )?;
        created += 1;

        // Point this memory back at the bytes it came from, so a caller can
        // recover the tool_use/thinking blocks `text_of` dropped.
        if spans_addressable {
            if let Some(Some((start, end))) = spans.get(chunk_index) {
                crate::archive::record_span(conn, &memory_id, &import_id, *start, *end)?;
            }
        }

        // Each wikilink in this chunk becomes an entity the memory mentions,
        // which is what makes the vault's own link graph traversable. Entity
        // upsert creates the entity when it does not exist, so a link to a
        // note that has not been imported yet needs no special handling — it
        // resolves exactly like any other.
        if let Some(mentions) = chunk_extras.map(|e| &e.mention_entities) {
            for title in mentions {
                let entity = upsert_entity(
                    conn,
                    &EntityInput {
                        name: title.clone(),
                        kind: None,
                        aliases: Vec::new(),
                    },
                )?;
                crate::entity::link_memory_entity(conn, &memory_id, &entity.id)?;
            }
        }
    }

    let mut stats = if graph_records.is_empty() {
        ImportStats::default()
    } else {
        restore_graph_records(conn, &graph_records)?
    };
    stats.memories_created = created;
    stats.raw_entries = raw_entries;

    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at, stats)
         VALUES (?, ?, ?, ?, ?)",
        params![
            import_id,
            filename,
            hash,
            now,
            serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
        ],
    )?;

    Ok(ImportOutcome::Imported {
        import_id,
        kind: effective,
        file: filename.to_string(),
        stats,
    })
}

fn existing_import(conn: &Connection, hash: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT import_id FROM chat_imports WHERE hash = ?",
        params![hash],
        |r| r.get(0),
    )
    .optional()
}

/// Reject a kind/suffix pair the importer cannot honour.
///
/// Returns the refusal, or `None` when the pair is importable. Shared by every
/// entry point — a file path, a directory sweep, and a pushed payload — so the
/// set of formats this accepts is decided in one place. A webhook push in
/// particular arrives with a caller-supplied `filename` that names nothing on
/// disk, and it must be held to exactly the same rule as a real file.
pub fn validate_kind_and_suffix(
    kind: ImportKind,
    suffix: &str,
    filename: &str,
) -> Option<ImportOutcome> {
    if !SUPPORTED_SUFFIXES.contains(&suffix) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!("unsupported format: .{}", suffix),
        });
    }
    if kind == ImportKind::Pdf && suffix != "pdf" {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!("pdf import does not support .{}", suffix),
        });
    }
    if kind == ImportKind::Image && !IMAGE_SUFFIXES.contains(&suffix) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(
                "image import does not support .{}: use .png, .jpg or .jpeg",
                suffix
            ),
        });
    }
    if kind == ImportKind::Audio && !AUDIO_SUFFIXES.contains(&suffix) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(
                "audio import does not support .{}: use .mp3, .m4a, .wav or .ogg",
                suffix
            ),
        });
    }
    // The other direction: these bytes have no text reading at all, so a
    // request to parse them as chat or prose is a mistake worth naming rather
    // than a lossy decode worth attempting.
    if IMAGE_SUFFIXES.contains(&suffix) && !matches!(kind, ImportKind::Auto | ImportKind::Image) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(".{} files must be imported as an image", suffix),
        });
    }
    if AUDIO_SUFFIXES.contains(&suffix) && !matches!(kind, ImportKind::Auto | ImportKind::Audio) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(".{} files must be imported as audio", suffix),
        });
    }
    if kind == ImportKind::Readwise && suffix != "json" {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(
                "readwise import does not support .{}: a Readwise export is .json",
                suffix
            ),
        });
    }
    if kind == ImportKind::Obsidian && !matches!(suffix, "md" | "markdown") {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(
                "obsidian import does not support .{}: a vault note is Markdown",
                suffix
            ),
        });
    }
    if kind == ImportKind::Document && !DOCUMENT_SUFFIXES.contains(&suffix) {
        return Some(ImportOutcome::Failed {
            file: filename.to_string(),
            reason: format!(
                "document import does not support .{}: use .md, .markdown or .txt",
                suffix
            ),
        });
    }
    None
}

/// Import bytes already held in memory.
///
/// The filesystem-free entry point, for content that arrives over the network
/// rather than as a file: a pushed payload has no path to read. `filename` is
/// a *display name* — its extension picks the parser exactly as a real file's
/// would, and it is stored in each memory's metadata and the `chat_imports`
/// row, but nothing resolves it against the filesystem.
///
/// That is the reason this is a separate entry point rather than a temporary
/// file: writing a pushed payload to disk to import it would put attacker-
/// controlled bytes under a real path, and the import roots exist precisely to
/// keep that from happening.
///
/// Deduplicates by content hash like a file import, so pushing byte-identical
/// content twice is a no-op.
#[allow(clippy::too_many_arguments)]
pub fn import_bytes(
    conn: &Connection,
    content: &[u8],
    filename: &str,
    category: &str,
    tags: &[String],
    extract_mode: &str,
    max_length: usize,
    kind: ImportKind,
) -> Result<ImportOutcome> {
    let suffix = suffix_of(std::path::Path::new(filename));

    if let Some(rejection) = validate_kind_and_suffix(kind, &suffix, filename) {
        return Ok(rejection);
    }

    let hash = hash_bytes(content);
    if let Some(import_id) = existing_import(conn, &hash)? {
        return Ok(ImportOutcome::Skipped {
            reason: "already_imported".to_string(),
            file: filename.to_string(),
            import_id,
        });
    }

    let raw = String::from_utf8_lossy(content).to_string();
    import_content(
        conn,
        &raw,
        content,
        &suffix,
        filename,
        &hash,
        category,
        tags,
        extract_mode,
        max_length,
        kind,
    )
}

/// Import one already-validated file.
///
/// The dedup hash is computed from the file's bytes and checked **before** the
/// text is read or parsed, so re-importing an unchanged file costs a hash and
/// a lookup rather than a full parse.
#[allow(clippy::too_many_arguments)]
pub fn import_file(
    conn: &Connection,
    path: &std::path::Path,
    category: &str,
    tags: &[String],
    extract_mode: &str,
    max_length: usize,
    kind: ImportKind,
) -> Result<ImportOutcome> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let suffix = suffix_of(path);

    if let Some(rejection) = validate_kind_and_suffix(kind, &suffix, &filename) {
        return Ok(rejection);
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(ImportOutcome::Failed {
                file: filename,
                reason: e.to_string(),
            })
        }
    };
    let hash = hash_bytes(&bytes);

    if let Some(import_id) = existing_import(conn, &hash)? {
        return Ok(ImportOutcome::Skipped {
            reason: "already_imported".to_string(),
            file: filename,
            import_id,
        });
    }

    let raw = String::from_utf8_lossy(&bytes).to_string();
    import_content(
        conn,
        &raw,
        &bytes,
        &suffix,
        &filename,
        &hash,
        category,
        tags,
        extract_mode,
        max_length,
        kind,
    )
}

/// Import one file named by a caller.
///
/// Validates the path against the import roots — containment before existence
/// — then delegates to [`import_file`].
pub fn import_chat(conn: &Connection, input: &ChatImportInput) -> Result<ImportOutcome> {
    let path = match validate_import_file(&input.file_path) {
        Ok(path) => path,
        Err(e) => {
            return Ok(ImportOutcome::Failed {
                file: input.file_path.clone(),
                reason: e.to_string(),
            })
        }
    };
    import_file(
        conn,
        &path,
        &input.category,
        &input.tags,
        &input.extract_mode,
        input
            .max_length
            .clamp(IMPORT_MAX_LENGTH_MIN, IMPORT_MAX_LENGTH_MAX),
        input.kind,
    )
}

/// Import every supported file in a directory.
///
/// Files are visited in sorted order so a run is reproducible. An
/// unsupported extension is not an error — a notes folder holding a stray
/// `.png` should import the markdown beside it rather than refusing the lot —
/// so those are skipped silently and never counted as seen.
pub fn import_directory(conn: &Connection, input: &BulkImportDirInput) -> Result<BulkImportResult> {
    let root = match validate_import_dir(&input.directory) {
        Ok(root) => root,
        Err(e) => {
            return Ok(BulkImportResult {
                files_failed: 1,
                results: vec![ImportOutcome::Failed {
                    file: input.directory.clone(),
                    reason: e.to_string(),
                }],
                ..Default::default()
            })
        }
    };

    let mut files = Vec::new();
    collect_files(&root, input.recursive, &mut files);
    files.sort();

    let max_length = input
        .max_length
        .clamp(IMPORT_MAX_LENGTH_MIN, IMPORT_MAX_LENGTH_MAX);
    let mut result = BulkImportResult::default();
    for path in files {
        result.files_seen += 1;
        let outcome = import_file(
            conn,
            &path,
            &input.category,
            &input.tags,
            &input.extract_mode,
            max_length,
            input.kind,
        )?;
        match &outcome {
            ImportOutcome::Imported { stats, .. } => {
                result.files_imported += 1;
                result.memories_created += stats.memories_created;
            }
            ImportOutcome::Skipped { .. } => result.files_skipped += 1,
            ImportOutcome::Failed { .. } => result.files_failed += 1,
        }
        result.results.push(outcome);
    }
    Ok(result)
}

fn collect_files(dir: &std::path::Path, recursive: bool, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_files(&path, recursive, out);
            }
        } else if SUPPORTED_SUFFIXES.contains(&suffix_of(&path).as_str()) {
            out.push(path);
        }
    }
}

/// A registered import parser.
///
/// Broader than the kinds [`ImportKind`] accepts: a connector can be
/// registered purely so it is discoverable, while its actual ingestion runs
/// through its own dedicated tool. `mempalace` is the reference's example —
/// listed, but never reached through `remind_me_import_chat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorInfo {
    pub kind: String,
    pub description: String,
    /// Extensions this connector parses, empty when it does not read files.
    pub suffixes: Vec<String>,
    /// Whether `remind_me_import_chat`'s `kind` parameter accepts it.
    pub file_import_kind: bool,
}

/// Every registered connector.
///
/// A fixed list rather than a runtime registry: this crate has no plugin
/// mechanism, so a mutable registry would be indirection with exactly one
/// possible set of contents. If connectors ever become pluggable, this is the
/// function that grows a registry behind it.
pub fn connectors() -> Vec<ConnectorInfo> {
    vec![
        ConnectorInfo {
            kind: "chat".to_string(),
            description: "Chat exports: JSON, JSONL, or markdown with role markers. \
                          Extracts messages per `extract_mode` and chunks per message."
                .to_string(),
            suffixes: SUPPORTED_SUFFIXES.iter().map(|s| s.to_string()).collect(),
            file_import_kind: true,
        },
        ConnectorInfo {
            kind: "document".to_string(),
            description: "Notes and documents. Chunks per Markdown section, carrying the \
                          heading breadcrumb, or per paragraph for plain text."
                .to_string(),
            suffixes: DOCUMENT_SUFFIXES.iter().map(|s| s.to_string()).collect(),
            file_import_kind: true,
        },
        // Listed for discovery, not for dispatch. `remind_me_import_dbs` reads
        // SQL rather than parsing a file, so it has its own entry point and
        // its own per-item dedup loop — `file_import_kind: false` is what says
        // "you cannot pass this as `kind` to remind_me_import_chat", which is
        // the question a caller reading this list is actually asking.
        ConnectorInfo {
            kind: "dbs".to_string(),
            description: "A daily-backup-system archive. Reads its items/sources tables \
                          directly, read-only, preserving each item's source and tags as \
                          knowledge-graph entities. Use remind_me_import_dbs."
                .to_string(),
            suffixes: Vec::new(),
            file_import_kind: false,
        },
        // Same rationale as "dbs" above: listed so a caller browsing this
        // list learns what "parsing a drawer" means (frontmatter-restore or
        // opaque), without implying `remind_me_import_chat` can dispatch to
        // it. The real ingestion path (`remind_me_import_mempalace`) keeps
        // its own bespoke per-drawer dedup/paging loop, reading Chroma's
        // metadata segment directly rather than a file.
        ConnectorInfo {
            kind: "mempalace".to_string(),
            description: "A MemPalace ChromaDB store. Reads its metadata segment directly, \
                          read-only. A drawer carrying remind_me's own frontmatter has its \
                          category/tags/created restored; everything else is stored opaquely, \
                          tagged with wing and room. Use remind_me_import_mempalace."
                .to_string(),
            suffixes: Vec::new(),
            file_import_kind: false,
        },
    ]
}
