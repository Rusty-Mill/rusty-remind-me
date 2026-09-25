//! Setting, clearing and listing time-based reminders.
//!
//! Delivery is deliberately not here: this module only decides *what is due*.
//! The background loop that actually fires a due reminder is a separate seam,
//! and keeping the window definition on this side means the scheduler, the
//! listing tool and the digest cannot drift into three different opinions
//! about what "overdue" means — the reference factored its own window SQL out
//! of `tools/reminders.py` for exactly that reason once its digest needed it.
//!
//! # Why a past timestamp is rejected rather than stored
//!
//! A reminder set for a moment that has already passed can never fire in the
//! ordinary sense — it would land straight in the overdue pile, which is the
//! bucket meaning "the scheduler was down when this came due". Storing it
//! would put a user's typo in the same place as a genuine missed delivery, so
//! it fails loudly at the call instead.
//!
//! # Windows
//!
//! - `upcoming` — a set reminder still in the future.
//! - `overdue` — due, and with no matching `reminder_deliveries` row. Delivery
//!   is keyed on `(memory_id, remind_at)`, so *rescheduling* a delivered
//!   reminder makes it pending again rather than staying suppressed by the old
//!   delivery.
//! - `all` — the union.
//!
//! Both exclude tombstones. A deleted memory's reminder firing would surface
//! content the user deleted.

use crate::db::queries::{parse_memory_row, prefixed_memory_columns};
use crate::models::{Memory, ReminderWindow, SetReminderOutcome};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// Parse an ISO-8601 timestamp the way the reference's `datetime.fromisoformat`
/// does, and canonicalize it to UTC.
///
/// Naive timestamps are read as UTC rather than as local time. Local would be
/// friendlier to type and worse to store: the same string would mean different
/// instants on two synced machines, and the column is compared against a UTC
/// `now` everywhere it is read.
pub fn parse_remind_at(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }

    // The naive shapes `fromisoformat` accepts. Date-only lands at midnight,
    // which is what Python does too.
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
    }

    None
}

/// Set a memory's reminder, or clear it when `remind_at` is `None`.
///
/// Bumps `updated_at`, which is what puts the change in the sync outbox and
/// what LWW compares. It deliberately writes **no revision**: the revision log
/// exists to recover a value a human replaced in the memory's content, and a
/// vault whose history is half reminder-scheduling noise is harder to read
/// back than one that only records edits.
pub fn set_reminder(
    conn: &Connection,
    memory_id: &str,
    remind_at: Option<&str>,
) -> Result<SetReminderOutcome> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM memories WHERE id = ? AND deleted_at IS NULL",
            params![memory_id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(SetReminderOutcome::NotFound {
            memory_id: memory_id.to_string(),
        });
    }

    let now = Utc::now();

    // An omitted, null, or whitespace-only value all mean "clear", matching
    // the reference: a blank string arriving from a form field is not a
    // timestamp, and rejecting it would make clearing awkward to express.
    let Some(raw) = remind_at.map(str::trim).filter(|r| !r.is_empty()) else {
        conn.execute(
            "UPDATE memories SET remind_at = NULL, updated_at = ? WHERE id = ?",
            params![now.to_rfc3339(), memory_id],
        )?;
        return Ok(SetReminderOutcome::Cleared {
            memory_id: memory_id.to_string(),
        });
    };

    let Some(when) = parse_remind_at(raw) else {
        return Ok(SetReminderOutcome::Rejected {
            reason: format!("'{}' is not a valid ISO-8601 timestamp", raw),
        });
    };
    if when <= now {
        return Ok(SetReminderOutcome::Rejected {
            reason: format!(
                "remind_at must be in the future, got {} (now: {})",
                when.to_rfc3339(),
                now.to_rfc3339()
            ),
        });
    }

    let stored = when.to_rfc3339();
    conn.execute(
        "UPDATE memories SET remind_at = ?, updated_at = ? WHERE id = ?",
        params![stored, now.to_rfc3339(), memory_id],
    )?;
    Ok(SetReminderOutcome::Set {
        memory_id: memory_id.to_string(),
        remind_at: stored,
    })
}

/// The SQL fragment for one window, and how many `now` bindings it consumes.
fn window_sql(when: ReminderWindow) -> (String, usize) {
    let not_delivered = "NOT EXISTS (SELECT 1 FROM reminder_deliveries rd \
         WHERE rd.memory_id = m.id AND rd.remind_at = m.remind_at)";
    let upcoming = "m.remind_at > ?";
    let overdue = format!("(m.remind_at <= ? AND {})", not_delivered);

    match when {
        ReminderWindow::Upcoming => (upcoming.to_string(), 1),
        ReminderWindow::Overdue => (overdue, 1),
        ReminderWindow::All => (format!("({} OR {})", upcoming, overdue), 2),
    }
}

/// Memories with a reminder set, filtered to a window, soonest first.
///
/// Shared with the digest rather than re-derived there, so the two can never
/// disagree about what is overdue.
pub fn list_reminders(conn: &Connection, when: ReminderWindow, limit: i64) -> Result<Vec<Memory>> {
    let now = Utc::now().to_rfc3339();
    let (window, now_bindings) = window_sql(when);

    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories m
          WHERE m.remind_at IS NOT NULL
            AND m.deleted_at IS NULL
            AND {}
          ORDER BY m.remind_at ASC
          LIMIT ?",
        prefixed_memory_columns("m"),
        window
    ))?;

    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    for _ in 0..now_bindings {
        bindings.push(rusqlite::types::Value::Text(now.clone()));
    }
    bindings.push(rusqlite::types::Value::Integer(limit));

    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(bindings.iter()),
            parse_memory_row,
        )?
        .collect::<Result<Vec<_>>>()?;
    Ok(rows)
}

/// Render memories the way the reference's `_fmt_memory_md` does.
///
/// Kept byte-compatible with the reference's layout — including the 2000-char
/// content truncation and the lock marker — because this text is what a model
/// reads back, and a different shape is a different prompt.
pub fn render_memories_markdown(memories: &[Memory]) -> String {
    if memories.is_empty() {
        return "_No memories found._".to_string();
    }
    memories
        .iter()
        .map(render_memory_markdown)
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// Render a `remind_me_list` page the way the reference's `_fmt_memories`
/// does when it is given a `total`: a `**Showing N of M memories**` header
/// ahead of the same per-memory rendering, joined by the same `\n---\n`
/// separator. `remind_me_list_reminders` calls `_fmt_memories` *without* a
/// `total` (reminders.py:116), which is why it stays on
/// `render_memories_markdown` alone rather than sharing this wrapper — only
/// `remind_me_list` (crud.py:189-196) passes one.
pub fn render_memory_page_markdown(memories: &[Memory], total: usize) -> String {
    if memories.is_empty() {
        return "_No memories found._".to_string();
    }
    let header = format!("**Showing {} of {} memories**\n", memories.len(), total);
    std::iter::once(header)
        .chain(memories.iter().map(render_memory_markdown))
        .collect::<Vec<_>>()
        .join("\n---\n")
}

fn render_memory_markdown(m: &Memory) -> String {
    let tags = if m.tags.is_empty() {
        "none".to_string()
    } else {
        m.tags.join(", ")
    };
    let mut lines = vec![
        format!(
            "### Memory `{}`{}",
            m.id,
            if m.sensitive { " 🔒 _sensitive_" } else { "" }
        ),
        format!(
            "**Category:** {}  |  **Tags:** {}  |  **Source:** {}",
            m.category, tags, m.source
        ),
    ];

    if let Some(object) = m.metadata.as_object() {
        if !object.is_empty() {
            let rendered = object
                .iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => format!("{}={}", k, s),
                    other => format!("{}={}", k, other),
                })
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("**Metadata:** {}", rendered));
        }
    }

    lines.push(format!(
        "**Created:** {}  |  **Updated:** {}",
        m.created_at, m.updated_at
    ));
    if let Some(remind_at) = &m.remind_at {
        lines.push(format!("**Reminder:** {}", remind_at));
    }
    lines.push(String::new());

    // Truncated by character, not byte, so a multi-byte boundary cannot be
    // split — the reference slices a Python str, where the same is free.
    let content: String = m.content.chars().take(2000).collect();
    let ellipsis = if m.content.chars().count() > 2000 {
        "…"
    } else {
        ""
    };
    lines.push(format!("{}{}", content, ellipsis));
    lines.push(String::new());

    lines.join("\n")
}
