//! Obsidian vault import.
//!
//! Understands the three Obsidian-flavoured Markdown conventions a generic
//! document import treats as opaque prose:
//!
//! - **YAML frontmatter** — parsed into fields, with `tags` folded into the
//!   memory's tags exactly like an inline `#tag`.
//! - **`[[Wikilinks]]`** — each resolves to an **entity** for the linked
//!   note's title, and the mentioning memory is linked to it. That is what
//!   makes the vault's own link graph queryable through `remind_me_entity` /
//!   `remind_me_entity_traverse` instead of being flattened into text. Note
//!   that this is the *entity* graph, not the `wiki_links` table — that
//!   belongs to the separate LLM Wiki layer and is untouched here.
//! - **Inline `#tag`** — folded into the memory's tags, deduplicated against
//!   the frontmatter ones.
//!
//! # Chunking is not reimplemented
//!
//! This strips the frontmatter block and hands the body to
//! [`crate::importer::parse_document`], the same per-section Markdown chunker
//! the plain document kind uses. Only the per-note extraction is new.
//!
//! # Frontmatter parsing is hand-rolled, and deliberately partial
//!
//! Real Obsidian frontmatter is overwhelmingly flat: `key: value`,
//! `key: [a, b]`, or `key:` followed by indented `- item` lines. Full YAML's
//! nested maps, anchors, multi-document streams and block scalars are
//! vanishingly rare in a vault. A small parser covering exactly that shape is
//! preferable to pulling a YAML crate into the workspace for this bounded a
//! need — the same call the reference made, and the same minimal-dependency
//! stance the rest of this crate takes.
//!
//! It never fails on what it cannot parse. Any unrecognised construct makes
//! the whole block degrade to "no fields extracted" rather than erroring, and
//! the delimiters are stripped from the body either way — so a note with
//! exotic frontmatter still imports its prose cleanly, rather than either
//! failing or storing raw `---` markers.
//!
//! # v1 limitation, stated rather than hidden
//!
//! A heading or block anchor is stripped and discarded: `[[Note#Heading]]`
//! resolves to an entity for `Note` as a whole, the same entity a plain
//! `[[Note]]` would. Resolving to a specific section would need a section
//! identity the entity graph does not have.

use serde_json::{Map, Value};

/// `memories.source` for Obsidian imports.
pub const OBSIDIAN_SOURCE: &str = "obsidian_import";

/// Default `memories.category`, kept distinct from the generic document one so
/// a search can filter specifically on notes ingested from a vault.
pub const OBSIDIAN_CATEGORY: &str = "obsidian";

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

/// Coerce one YAML-ish scalar token to a JSON value.
///
/// Quoted strings, booleans, null and plain numbers. Anything else stays a
/// trimmed string — a deliberately small subset of YAML scalar resolution,
/// covering the shapes that actually appear in a vault.
fn parse_scalar(raw: &str) -> Value {
    let v = raw.trim();
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        let first = bytes[0] as char;
        if (first == '\'' || first == '"') && v.ends_with(first) {
            return Value::String(v[1..v.len() - 1].to_string());
        }
    }
    match v.to_ascii_lowercase().as_str() {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" | "" => return Value::Null,
        _ => {}
    }
    if let Ok(i) = v.parse::<i64>() {
        return Value::Number(i.into());
    }
    // Only `-?\d+\.\d+`, not everything `f64::from_str` accepts — "1e5" and
    // "inf" are far more likely to be prose than numbers in a note's
    // frontmatter.
    if looks_like_decimal(v) {
        if let Ok(f) = v.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Value::Number(n);
            }
        }
    }
    Value::String(v.to_string())
}

fn looks_like_decimal(v: &str) -> bool {
    let body = v.strip_prefix('-').unwrap_or(v);
    match body.split_once('.') {
        Some((int, frac)) => {
            !int.is_empty()
                && !frac.is_empty()
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Is this a bare `key:` line, and what are its parts?
fn key_line(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((key, rest.trim()))
}

/// Parse a flat `key: value` / `key: [list]` / `key:` + `- item` block.
///
/// Returns an empty map the moment a line matches none of those three shapes.
/// That is the degrade-gracefully contract: partial extraction from a
/// frontmatter block this cannot fully read would be worse than none, because
/// a caller cannot tell which half it got.
fn parse_simple_yaml(lines: &[&str]) -> Map<String, Value> {
    let mut result = Map::new();
    let mut key: Option<String> = None;
    let mut list_open = false;

    for raw_line in lines {
        if raw_line.trim().is_empty() {
            continue;
        }

        // Indented: a list item under the current key, or something nested.
        if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            let trimmed = raw_line.trim_start();
            let Some(item) = trimmed.strip_prefix('-') else {
                return Map::new(); // nested mapping
            };
            let Some(owner) = key.as_ref() else {
                return Map::new(); // an item with no owning key
            };
            if !list_open {
                result.insert(owner.clone(), Value::Array(Vec::new()));
                list_open = true;
            }
            if let Some(Value::Array(items)) = result.get_mut(owner) {
                items.push(parse_scalar(item));
            }
            continue;
        }

        let Some((k, value)) = key_line(raw_line.trim_end()) else {
            return Map::new();
        };
        key = Some(k.to_string());
        list_open = false;

        if value.is_empty() {
            // Either a bare null, or a block list on the following indented
            // lines — the branch above replaces this when one arrives.
            result.insert(k.to_string(), Value::Null);
            continue;
        }
        if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            let items = inner
                .split(',')
                .filter(|x| !x.trim().is_empty())
                .map(parse_scalar)
                .collect();
            result.insert(k.to_string(), Value::Array(items));
            continue;
        }
        // Flow mapping, anchor, alias or tag — all unsupported, and all
        // meaning the block as a whole is beyond this parser.
        if value.starts_with('{') || value.contains(['&', '*', '!']) {
            return Map::new();
        }
        result.insert(k.to_string(), parse_scalar(value));
    }
    result
}

/// Split a leading frontmatter block off `text`, returning `(fields, body)`.
///
/// `fields` is empty when there is no block, when the block has no closing
/// delimiter (a note that merely starts with a horizontal rule is not a note
/// with frontmatter), or when its content is beyond [`parse_simple_yaml`].
/// `body` always has the delimited block removed when one was found.
pub fn parse_frontmatter(text: &str) -> (Map<String, Value>, String) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (Map::new(), text.to_string());
    }

    let Some(end) = lines.iter().skip(1).position(|l| l.trim() == "---") else {
        return (Map::new(), text.to_string());
    };
    let end = end + 1;

    let fields = parse_simple_yaml(&lines[1..end]);
    let body = lines[end + 1..]
        .join("\n")
        .trim_start_matches('\n')
        .to_string();
    (fields, body)
}

/// Pull a frontmatter `tags` field out as a flat list.
///
/// Accepts Obsidian's two shapes: a YAML list, and a single string that may be
/// comma-separated.
pub fn frontmatter_tags(fields: &Map<String, Value>) -> Vec<String> {
    match fields.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(scalar_to_string)
            .filter(|t| !t.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        Some(other) => {
            let s = scalar_to_string(other);
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s]
            }
        }
    }
}

fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Wikilinks
// ---------------------------------------------------------------------------

/// One `[[Note]]` occurrence: the resolved title, the alias if any, and the
/// literal matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wikilink {
    pub title: String,
    pub alias: String,
    /// The raw `[[…]]` text, used to decide which chunk the mention landed in.
    pub raw: String,
    pub start: usize,
    pub end: usize,
}

/// Extract `[[Note]]`, `[[Note|Alias]]` and `[[Note#Heading]]` links.
///
/// Every occurrence in document order, **not** deduplicated: a caller asking
/// which chunk contains a given link needs each one separately, and the
/// unique-titles view is one `dedupe_ci` away.
///
/// A trailing `#heading` or `^block-id` anchor is stripped before the
/// remainder is treated as the note title — the v1 limitation stated in the
/// module docs.
pub fn parse_wikilinks(text: &str) -> Vec<Wikilink> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i + 3 < bytes.len() {
        if bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        // Scan to the closing `]]`. A newline ends the candidate: an unclosed
        // `[[` should not swallow the rest of the note.
        let Some(close) = find_close(text, i + 2) else {
            i += 2;
            continue;
        };

        let inner = &text[i + 2..close];
        let (target, alias) = match inner.split_once('|') {
            Some((t, a)) => (t.trim(), a.trim()),
            None => (inner.trim(), ""),
        };
        // Cut at whichever anchor marker comes first: `#heading` and `^block`
        // are both anchors, and a title can legitimately contain neither.
        let cut = target
            .find(['#', '^'])
            .map(|at| &target[..at])
            .unwrap_or(target);
        let title = cut.trim();

        if !title.is_empty() {
            out.push(Wikilink {
                title: title.to_string(),
                alias: alias.to_string(),
                raw: text[i..close + 2].to_string(),
                start: i,
                end: close + 2,
            });
        }
        i = close + 2;
    }
    out
}

fn find_close(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == b']' && bytes[i + 1] == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Inline tags
// ---------------------------------------------------------------------------

/// Extract inline `#tag` / `#nested/tag` references.
///
/// Four things are deliberately **not** tags, and each is a real shape that
/// appears in a vault:
///
/// - `# Heading` — the space after `#` disqualifies it;
/// - `[[Note#Heading]]` — wikilink spans are masked out first;
/// - a `#` inside fenced or inline code — those spans are masked too;
/// - a purely numeric `#123` — Obsidian does not allow one, and it is far
///   more likely to be an issue reference.
///
/// Returns tags in first-appearance order, without the `#`, deduplicated
/// case-insensitively with the first-seen casing kept.
pub fn extract_inline_tags(text: &str, exclude: &[(usize, usize)]) -> Vec<String> {
    let masked = mask(text, exclude);
    let bytes = masked.as_bytes();

    let mut seen = std::collections::HashSet::new();
    let mut tags = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'#' {
            i += 1;
            continue;
        }
        // `(?<![\w#/])#` — a `#` preceded by a word character, another `#`, or
        // a slash is a continuation of something else, not a tag opener.
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'#' || prev == b'/' {
                i += 1;
                continue;
            }
        }
        let start = i + 1;
        let mut end = start;
        // First character cannot be `-` or `/`, matching `[A-Za-z0-9_]`.
        if end >= bytes.len() || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            i += 1;
            continue;
        }
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric()
                || bytes[end] == b'_'
                || bytes[end] == b'-'
                || bytes[end] == b'/')
        {
            end += 1;
        }
        let tag = &masked[start..end];
        if !tag.replace('/', "").bytes().all(|b| b.is_ascii_digit()) {
            let key = tag.to_ascii_lowercase();
            if seen.insert(key) {
                tags.push(tag.to_string());
            }
        }
        i = end;
    }
    tags
}

/// Blank out each span with spaces, keeping newlines and every other
/// character's position so spans found against the original still line up.
fn mask(text: &str, extra: &[(usize, usize)]) -> String {
    let mut spans: Vec<(usize, usize)> = code_spans(text);
    spans.extend_from_slice(extra);
    if spans.is_empty() {
        return text.to_string();
    }

    let mut bytes = text.as_bytes().to_vec();
    for (start, end) in spans {
        for b in bytes.iter_mut().take(end.min(text.len())).skip(start) {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    // Masking only ever replaces whole ASCII bytes with a space, so any
    // multi-byte sequence is either fully inside a span or fully outside it.
    String::from_utf8(bytes).unwrap_or_else(|_| text.to_string())
}

/// Spans of fenced blocks and inline code, which a `#` inside must not escape.
fn code_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();

    // Fenced blocks first: an inline-code scan inside a fence would find
    // spurious pairs, and the fence span covers them anyway.
    let mut fence: Option<(usize, usize, u8)> = None; // (content start, fence len, char)
    let mut line_start = 0;
    while line_start <= bytes.len() {
        let line_end = text[line_start..]
            .find('\n')
            .map(|n| line_start + n)
            .unwrap_or(bytes.len());
        let line = text[line_start..line_end].trim_start();
        let marker = line.as_bytes().first().copied();

        if matches!(marker, Some(b'`') | Some(b'~')) {
            let ch = marker.unwrap();
            let run = line.bytes().take_while(|b| *b == ch).count();
            if run >= 3 {
                match fence {
                    Some((open, open_run, open_ch)) if open_ch == ch && run >= open_run => {
                        spans.push((open, line_end));
                        fence = None;
                    }
                    None => fence = Some((line_start, run, ch)),
                    _ => {}
                }
            }
        }

        if line_end >= bytes.len() {
            break;
        }
        line_start = line_end + 1;
    }
    // An unterminated fence runs to the end of the note.
    if let Some((open, _, _)) = fence {
        spans.push((open, bytes.len()));
    }

    // Inline code, skipping anything already covered by a fence.
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' || spans.iter().any(|(s, e)| i >= *s && i < *e) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b'`' && bytes[j] != b'\n' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'`' {
            spans.push((i, j + 1));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    spans
}

/// Case-insensitive dedup, first-seen casing kept, order preserved.
pub fn dedupe_ci<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| !item.is_empty() && seen.insert(item.to_ascii_lowercase()))
        .collect()
}

// ---------------------------------------------------------------------------
// The connector
// ---------------------------------------------------------------------------

/// One chunk of an Obsidian note, with what the ingest path needs to attach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObsidianChunk {
    pub content: String,
    pub section: Option<String>,
    /// Frontmatter and inline tags combined, to merge into the memory's tags.
    pub extra_tags: Vec<String>,
    /// Note titles to resolve into entities and link this memory to.
    pub mention_entities: Vec<String>,
}

/// Parse one note into chunks.
///
/// A chunk carries `mention_entities` only for the wikilinks whose literal
/// `[[…]]` text actually landed in it. Chunking never rewrites text, so the
/// markup survives verbatim into whichever chunk it fell into — which means a
/// mention is credited to the section that made it rather than smeared across
/// the whole file.
pub fn parse_note(raw: &str, max_length: usize) -> (Vec<ObsidianChunk>, Map<String, Value>) {
    let (fields, body) = parse_frontmatter(raw);

    let tags_from_frontmatter = frontmatter_tags(&fields);
    let mut frontmatter_extra = fields.clone();
    frontmatter_extra.remove("tags");

    let links = parse_wikilinks(&body);
    let spans: Vec<(usize, usize)> = links.iter().map(|l| (l.start, l.end)).collect();
    let inline = extract_inline_tags(&body, &spans);
    let combined = dedupe_ci(tags_from_frontmatter.into_iter().chain(inline));

    let chunks = crate::importer::parse_document(&body, "md", max_length)
        .into_iter()
        .map(|(content, section)| {
            let mentions = dedupe_ci(
                links
                    .iter()
                    .filter(|l| content.contains(&l.raw))
                    .map(|l| l.title.clone()),
            );
            ObsidianChunk {
                content,
                section,
                extra_tags: combined.clone(),
                mention_entities: mentions,
            }
        })
        .collect();

    (chunks, frontmatter_extra)
}
