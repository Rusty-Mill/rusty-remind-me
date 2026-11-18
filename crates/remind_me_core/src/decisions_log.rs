//! Decisions log export: one project's decision memories rendered as a
//! markdown table (card #1565, pack D18 to D21).
//!
//! A read-only view: nothing flows back from the file. The output is
//! byte-identical for the same memories, so a regenerated `DECISIONS.md`
//! diffs cleanly. That rules out a run-time date: the header carries the
//! newest `updated_at` among the rendered rows instead.
//!
//! Field sources (D19): number `metadata.decision_ref`, kept as written and
//! never generated; date `valid_from`, else `created_at`; title
//! `metadata.title`, else the first line of `content`; status `outcome`, else
//! `superseded` when superseded, else `open`.

use crate::db::memories::Memories;
use crate::db::Store;
use crate::models::Memory;
use chrono::{DateTime, Utc};

/// Longest title taken from `content`, in characters.
const TITLE_MAX_CHARS: usize = 100;

/// Placeholder for an empty cell.
const NONE: &str = "—";

/// Which decisions a log holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionsQuery {
    /// Matched against a memory's `project`, ignoring ASCII case.
    pub project: String,
    /// Keep superseded decisions, marked with what replaced them. Off by
    /// default: a log of live decisions.
    pub include_superseded: bool,
}

/// Read `query`'s decisions from `store` and render the log.
pub fn export(store: &Store<'_>, query: &DecisionsQuery) -> crate::db::Result<String> {
    let all = Memories::new(store).live_of_type("decision")?;
    Ok(render(&query.project, &select(&all, query)))
}

/// The decisions `query` takes from `memories`, in log order: by date, then
/// `created_at`, then id. Sensitive memories never reach a file.
pub fn select<'m>(memories: &'m [Memory], query: &DecisionsQuery) -> Vec<&'m Memory> {
    let mut picked: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.memory_type.as_deref() == Some("decision"))
        .filter(|m| m.deleted_at.is_none() && !m.sensitive)
        .filter(|m| query.include_superseded || m.superseded_by.is_none())
        .filter(|m| {
            m.project
                .as_deref()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case(query.project.trim()))
        })
        .collect();
    picked.sort_by(|a, b| (date(a), &a.created_at, &a.id).cmp(&(date(b), &b.created_at, &b.id)));
    picked
}

/// Render `decisions` (already in log order) as the project's log.
pub fn render(project: &str, decisions: &[&Memory]) -> String {
    let mut out = format!("# Decisions: {}\n\n", cell(project));
    out.push_str(&header_line(decisions));
    out.push_str("\n\n");
    if decisions.is_empty() {
        out.push_str("_No decisions recorded._\n");
        return out;
    }
    out.push_str("| D | Date | Decision | Rationale | Status | Superseded by | Source |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for memory in decisions {
        out.push_str(&row(memory, decisions));
    }
    out
}

fn header_line(decisions: &[&Memory]) -> String {
    match decisions.iter().map(|m| day(&m.updated_at)).max() {
        Some(newest) => format!("Generated from remind-me on {newest}; do not hand-edit."),
        None => "Generated from remind-me; do not hand-edit.".to_string(),
    }
}

fn row(memory: &Memory, all: &[&Memory]) -> String {
    let rationale = metadata_str(memory, "rationale").unwrap_or(NONE);
    format!(
        "| {} | {} | {} | {} | {} | {} | {} |\n",
        cell(metadata_str(memory, "decision_ref").unwrap_or(NONE)),
        date(memory),
        cell(&title(memory)),
        cell(rationale),
        cell(status(memory)),
        superseded_by(memory, all),
        source(memory),
    )
}

/// `valid_from` if set, else `created_at`, as a UTC `YYYY-MM-DD`.
fn date(memory: &Memory) -> String {
    day(memory.valid_from.as_deref().unwrap_or(&memory.created_at))
}

/// The UTC calendar day of an RFC 3339 time; the first ten characters of
/// anything else, so a malformed stamp still sorts and renders stably.
fn day(raw: &str) -> String {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(t) => t.with_timezone(&Utc).format("%Y-%m-%d").to_string(),
        Err(_) => raw.chars().take(10).collect(),
    }
}

fn title(memory: &Memory) -> String {
    if let Some(title) = metadata_str(memory, "title") {
        return title.to_string();
    }
    let first = memory.content.lines().next().unwrap_or("").trim();
    truncate_on_word(first, TITLE_MAX_CHARS)
}

/// `text` cut to at most `max` characters, at the last space when there is
/// one, with an ellipsis when anything was cut.
fn truncate_on_word(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let kept = cut.rfind(' ').map_or(cut.as_str(), |i| &cut[..i]);
    format!("{}…", kept.trim_end())
}

fn status(memory: &Memory) -> &str {
    match (memory.outcome.as_deref(), &memory.superseded_by) {
        (Some(outcome), _) => outcome,
        (None, Some(_)) => "superseded",
        (None, None) => "open",
    }
}

/// The replacing memory's id, led by its number when the log holds it.
fn superseded_by(memory: &Memory, all: &[&Memory]) -> String {
    let Some(id) = memory.superseded_by.as_deref() else {
        return NONE.to_string();
    };
    let number = all
        .iter()
        .find(|m| m.id == id)
        .and_then(|m| metadata_str(m, "decision_ref"));
    match number {
        Some(n) => format!("{} ({})", cell(n), code(id)),
        None => code(id),
    }
}

fn source(memory: &Memory) -> String {
    let capture = memory
        .source_capture_id
        .as_deref()
        .or(memory.capture_id.as_deref());
    match capture {
        Some(c) => format!("{} · capture {}", code(&memory.id), code(c)),
        None => code(&memory.id),
    }
}

fn metadata_str<'m>(memory: &'m Memory, key: &str) -> Option<&'m str> {
    memory
        .metadata
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// An id as an inline code span, safe inside a table cell. Backticks are
/// dropped: they would end the span, and no id format uses them.
fn code(id: &str) -> String {
    format!("`{}`", cell(&id.replace('`', "")))
}

/// Text safe inside a table cell: newlines folded to spaces, `|` escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_folds_newlines_and_escapes_pipes() {
        assert_eq!(cell("a |\n b\r\nc"), "a \\| b c");
    }

    #[test]
    fn code_spans_survive_hostile_ids() {
        assert_eq!(code("mem_1"), "`mem_1`");
        assert_eq!(code("a`b|c\nd"), "`ab\\|c d`");
    }

    #[test]
    fn truncate_keeps_short_text_and_cuts_long_text_on_a_word() {
        assert_eq!(truncate_on_word("short", 10), "short");
        assert_eq!(truncate_on_word("alpha beta gamma", 12), "alpha beta…");
        assert_eq!(truncate_on_word("abcdefghijkl", 5), "abcde…");
    }

    #[test]
    fn day_normalises_to_utc_and_tolerates_malformed_stamps() {
        assert_eq!(day("2026-11-18T23:30:00-02:00"), "2026-11-19");
        assert_eq!(day("2026-11-18 10:00"), "2026-11-18");
    }
}
