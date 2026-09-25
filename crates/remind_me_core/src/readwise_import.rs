//! Readwise highlight export import.
//!
//! # File import, not an API client
//!
//! No live call is made against Readwise. The user exports once — via
//! `GET /api/v2/export/` or Readwise's own export tooling — saves the
//! response, and hands this the resulting file. That matches every other
//! connector here, and it keeps an access token out of this crate entirely.
//!
//! # Two input shapes
//!
//! The Export API returns an object: `{"count", "nextPageCursor", "results"}`,
//! where `results` is the array of book/article entries. A bare top-level
//! array of the same entries is also accepted, because someone may reasonably
//! have unwrapped `results` before saving. Anything else is refused with one
//! actionable message rather than failing downstream on a type mismatch.
//!
//! # One memory per highlight, not per book
//!
//! A highlight is Readwise's own atomic unit of meaning — nobody re-reads half
//! a highlight the way they might re-read half a document section. Grouping a
//! book's highlights into one chunked memory would make every search hit for
//! one highlight compete for ranking and embedding budget against every other
//! highlight in the same book, diluting exactly the retrieval precision a
//! memory store exists to provide.
//!
//! The book is not lost by that choice, only demoted: its title, author,
//! category, source URL and id ride as metadata on every highlight it
//! produced, so the context still travels alongside without shaping the
//! embedding.
//!
//! # A note is appended to content, never metadata-only
//!
//! `"{text}\n\nNote: {note}"`. The note is frequently *why* the highlight was
//! made at all, and FTS indexes `content`, not `metadata` — leaving it in
//! metadata would make the most valuable part of the record unsearchable.
//!
//! # Deliberately excluded from auto-detection
//!
//! A Readwise export and a chat export both arrive as an unadorned `.json`.
//! Content-sniffing works for chat *markdown* because role markers are a
//! strong, low-false-positive signal; JSON offers no equivalent. Sniffing for
//! a `highlights`-shaped key would misroute a chat export that merely
//! discusses Readwise — silently corrupting working chat-import behaviour,
//! which is strictly worse than requiring one explicit keyword.

use serde_json::{Map, Value};

/// `memories.source` for Readwise imports.
pub const READWISE_SOURCE: &str = "readwise_import";

/// Default `memories.category`, kept distinct so a search can filter on
/// highlights specifically.
pub const READWISE_CATEGORY: &str = "readwise";

/// Refusal for a file that parses as JSON but is not a Readwise export.
pub const READWISE_FORMAT_ERROR: &str =
    "Not a recognized Readwise export: expected a JSON object with a 'results' \
     array of book/article entries (the shape Readwise's Export API returns — \
     see https://readwise.io/api_deets), or a bare array of the same entries. \
     Each entry needs a 'highlights' array.";

/// One highlight, ready for ingest.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadwiseHighlight {
    pub content: String,
    pub metadata: Map<String, Value>,
}

/// Pull the book/article entries out of a parsed export.
///
/// Returns the refusal message rather than a partial list, because a file that
/// is not a Readwise export at all is a different problem from one with a few
/// bad rows, and the caller should hear about it differently.
fn extract_results(data: &Value) -> Result<Vec<&Value>, String> {
    if let Some(results) = data.get("results").and_then(|r| r.as_array()) {
        return Ok(results.iter().collect());
    }
    if let Some(array) = data.as_array() {
        return Ok(array.iter().collect());
    }
    Err(READWISE_FORMAT_ERROR.to_string())
}

/// Compose one highlight's content: the passage, plus its note when there is
/// one.
fn highlight_content(text: &str, note: Option<&Value>) -> String {
    let content = text.trim();
    match note.and_then(|n| n.as_str()).map(str::trim) {
        Some(note) if !note.is_empty() => format!("{}\n\nNote: {}", content, note),
        _ => content.to_string(),
    }
}

/// Insert `key` only when `value` is a non-blank string.
///
/// Sparse on purpose: only keys actually present are emitted, rather than
/// every key with a null placeholder. A reader can then tell "Readwise did not
/// have this" from "this is empty".
fn put_str(meta: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(s) = value.and_then(|v| v.as_str()) {
        let s = s.trim();
        if !s.is_empty() {
            meta.insert(key.to_string(), Value::String(s.to_string()));
        }
    }
}

/// Insert `key` when `value` is a number or a non-blank string.
///
/// Readwise sends ids and locations as either, depending on the field and the
/// export vintage, and both are worth keeping verbatim.
fn put_scalar(meta: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    match value {
        Some(Value::Number(n)) => {
            meta.insert(key.to_string(), Value::Number(n.clone()));
        }
        Some(Value::String(_)) => put_str(meta, key, value),
        _ => {}
    }
}

/// Build a highlight's metadata: the book's context plus the highlight's own
/// provenance.
pub fn highlight_metadata(entry: &Value, highlight: &Value) -> Map<String, Value> {
    let mut meta = Map::new();

    put_str(&mut meta, "readwise_title", entry.get("title"));
    put_str(&mut meta, "readwise_author", entry.get("author"));
    put_str(&mut meta, "readwise_category", entry.get("category"));
    put_str(&mut meta, "readwise_source_url", entry.get("source_url"));
    put_scalar(&mut meta, "readwise_book_id", entry.get("user_book_id"));

    put_scalar(&mut meta, "readwise_location", highlight.get("location"));
    put_str(
        &mut meta,
        "readwise_location_type",
        highlight.get("location_type"),
    );
    put_str(
        &mut meta,
        "readwise_highlighted_at",
        highlight.get("highlighted_at"),
    );
    put_scalar(&mut meta, "readwise_highlight_id", highlight.get("id"));
    put_str(&mut meta, "readwise_url", highlight.get("url"));

    // Readwise sends tags as `{"id", "name"}` objects; only the names are
    // useful here, and an empty list should produce no key at all.
    if let Some(tags) = highlight.get("tags").and_then(|t| t.as_array()) {
        let names: Vec<Value> = tags
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| Value::String(n.to_string()))
            .collect();
        if !names.is_empty() {
            meta.insert("readwise_tags".to_string(), Value::Array(names));
        }
    }

    meta
}

/// Parse a Readwise export into one entry per highlight.
///
/// Returns the highlights and the pre-chunk highlight count — the latter is
/// what "how many highlights were in this file" means, distinct from how many
/// memories the chunker produced from them.
///
/// A malformed *entry* or *highlight* is skipped rather than aborting the
/// import, matching the tolerance the chat connector already shows a bad JSONL
/// line. A malformed *top level* is refused outright: partway through is the
/// wrong place to discover the file was never a Readwise export.
pub fn parse_export(
    raw: &str,
    max_length: usize,
) -> Result<(Vec<ReadwiseHighlight>, usize), String> {
    let data: Value = serde_json::from_str(raw)
        .map_err(|e| format!("Could not parse Readwise export as JSON: {}", e))?;

    let entries = extract_results(&data)?;

    let mut out = Vec::new();
    let mut highlight_count = 0usize;

    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        let Some(highlights) = entry.get("highlights").and_then(|h| h.as_array()) else {
            continue;
        };

        for highlight in highlights {
            if !highlight.is_object() {
                continue;
            }
            let Some(text) = highlight.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }

            highlight_count += 1;
            let content = highlight_content(text, highlight.get("note"));
            let metadata = highlight_metadata(entry, highlight);

            // A highlight longer than the budget is chunked by the existing
            // chunker rather than truncated — a clipped passage read back
            // later gives no sign it was ever cut.
            for chunk in crate::importer::chunk_text(&content, max_length) {
                out.push(ReadwiseHighlight {
                    content: chunk,
                    metadata: metadata.clone(),
                });
            }
        }
    }

    Ok((out, highlight_count))
}
