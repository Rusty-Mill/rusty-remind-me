//! Vault digest — a periodic "what's happened lately" summary.
//!
//! # Sensitive memories are excluded with no override
//!
//! Unlike search and list, this has no `include_sensitive`. A digest is
//! precisely the ambient, passive surface the flag exists to keep sensitive
//! content off: it is often delivered on a schedule rather than read in
//! response to a specific question, so there is no per-call caller intent to
//! opt back in against. The reference makes the same call for the same reason.
//!
//! # Every section reads from the tool that owns it
//!
//! Reminders come from [`crate::reminders::list_reminders`] and sync health
//! from [`crate::sync::sync_status`] — the exact functions
//! `remind_me_list_reminders` and `remind_me_sync_status` call. That is what
//! makes it structurally impossible for the digest to disagree with them; a
//! second query here would be a second opinion about what "overdue" means.
//!
//! Both sections were previously `Option` and omitted, because their
//! subsystems did not exist yet and a "Reminders: none" line would have read
//! as "you have nothing due" when the truth was "nothing here can tell". Both
//! subsystems now exist (issues #116 and #114), so the honest shape is the
//! real answer.

use crate::models::{DigestData, DigestRecentMemory, DigestReminder, ReminderWindow, SyncStatus};
use crate::vitality::build_vitality_report;
use rusqlite::{params, Connection, Result};

/// Days back that count as "recent" by default.
pub const DEFAULT_SINCE_DAYS: i64 = 7;
/// Cap on the memories listed. `recent_total` carries the true count, so a
/// busy week reads as "20 of 340" rather than silently looking like 20.
pub const MAX_RECENT_MEMORIES: usize = 20;

pub const DIGEST_SINCE_DAYS_MIN: i64 = 1;
pub const DIGEST_SINCE_DAYS_MAX: i64 = 365;

/// Cap on the reminders listed per section.
pub const MAX_DIGEST_REMINDERS: i64 = 10;

/// Project a memory down to what the digest shows for a reminder.
fn digest_reminder(memory: &crate::models::Memory) -> DigestReminder {
    let mut content: String = memory.content.chars().take(120).collect();
    if memory.content.chars().count() > 120 {
        content.push('…');
    }
    DigestReminder {
        id: memory.id.clone(),
        remind_at: memory.remind_at.clone(),
        content,
    }
}

/// Assemble the digest's underlying data.
pub fn build_digest(conn: &Connection, since_days: i64) -> Result<DigestData> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(since_days)).to_rfc3339();

    let mut stmt = conn.prepare(
        "SELECT id, content, category, created_at
           FROM memories
          WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?
          ORDER BY created_at DESC
          LIMIT ?",
    )?;
    let recent_memories: Vec<DigestRecentMemory> = stmt
        .query_map(params![cutoff, MAX_RECENT_MEMORIES as i64], |r| {
            Ok(DigestRecentMemory {
                id: r.get(0)?,
                content: r.get(1)?,
                category: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    // Counted rather than taken from the capped list, so the cap is visible
    // rather than silently making a busy week look like a quiet one.
    let recent_total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories
          WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?",
        params![cutoff],
        |r| r.get(0),
    )?;

    Ok(DigestData {
        generated_at: chrono::Utc::now().to_rfc3339(),
        since_days,
        recent_memories,
        recent_total,
        vitality: build_vitality_report(conn)?,
        reminders_upcoming: crate::reminders::list_reminders(
            conn,
            ReminderWindow::Upcoming,
            MAX_DIGEST_REMINDERS,
        )?
        .iter()
        .map(digest_reminder)
        .collect(),
        reminders_overdue: crate::reminders::list_reminders(
            conn,
            ReminderWindow::Overdue,
            MAX_DIGEST_REMINDERS,
        )?
        .iter()
        .map(digest_reminder)
        .collect(),
        // Local-only, no network: the digest has to stay callable from a
        // scheduled path that cannot afford to block on a remote. The
        // hub-reconcile verdict is a network call and stays out.
        sync: crate::sync::sync_status(conn)?,
    })
}

/// Render a digest as Markdown.
///
/// Every section appears, and one with no data says so explicitly — "no new
/// memories this week" and "no reminders set" are both information, and a
/// section that silently vanished would read as "nothing happened" either way.
pub fn render_markdown(data: &DigestData) -> String {
    let mut out = String::from("# Vault digest\n\n");
    out.push_str(&format!(
        "_Generated {} · covering the last {} day{}_\n\n",
        data.generated_at,
        data.since_days,
        if data.since_days == 1 { "" } else { "s" }
    ));

    out.push_str("## Recent memories\n\n");
    if data.recent_memories.is_empty() {
        out.push_str("_Nothing new in this window._\n\n");
    } else {
        if data.recent_total > data.recent_memories.len() as i64 {
            out.push_str(&format!(
                "_Showing {} of {}._\n\n",
                data.recent_memories.len(),
                data.recent_total
            ));
        }
        for memory in &data.recent_memories {
            let mut snippet: String = memory.content.chars().take(120).collect();
            if memory.content.chars().count() > 120 {
                snippet.push('…');
            }
            out.push_str(&format!(
                "- **{}** ({}) — {}\n",
                memory.id, memory.category, snippet
            ));
        }
        out.push('\n');
    }

    let vitality = &data.vitality;
    out.push_str("## Vitality\n\n");
    out.push_str(&format!(
        "- Vault health: {}\n- Active: {} · Dormant: {}\n- Average vitality: {:.3}\n\n",
        vitality.vault_health_score,
        vitality.active_count,
        vitality.dormant_count,
        vitality.average_vitality
    ));

    out.push_str("## Reminders\n\n");
    if data.reminders_upcoming.is_empty() && data.reminders_overdue.is_empty() {
        out.push_str("_No reminders set._\n\n");
    } else {
        out.push_str(&format!(
            "**Upcoming:** {}  |  **Overdue:** {}\n",
            data.reminders_upcoming.len(),
            data.reminders_overdue.len()
        ));
        // Overdue first: something that should already have reached you and
        // did not is the more urgent of the two, and a reader who stops after
        // the first subsection should have stopped on that one.
        for (heading, reminders) in [
            ("Overdue", &data.reminders_overdue),
            ("Upcoming", &data.reminders_upcoming),
        ] {
            if reminders.is_empty() {
                continue;
            }
            out.push_str(&format!("\n### {}\n", heading));
            for reminder in reminders.iter() {
                out.push_str(&format!(
                    "- `{}` due {}: {}\n",
                    reminder.id,
                    reminder.remind_at.as_deref().unwrap_or("?"),
                    reminder.content
                ));
            }
        }
        out.push('\n');
    }

    out.push_str("## Sync health\n\n");
    match &data.sync {
        SyncStatus::Disabled { hint, .. } => {
            out.push_str(&format!("_Sync disabled — {}_\n", hint));
        }
        SyncStatus::Enabled {
            node_id,
            hub_url,
            outbox,
            remotes,
            ..
        } => {
            out.push_str(&format!(
                "Node `{}` → {}, outbox {} pending ({}).\n",
                node_id,
                hub_url,
                outbox.pending,
                format!("{:?}", outbox.drain).to_lowercase()
            ));
            for remote in remotes {
                let state = if remote.ever_contacted {
                    "ok"
                } else {
                    "never contacted"
                };
                out.push_str(&format!(
                    "- **{}**: {} pending — {}\n",
                    remote.remote_id, remote.pending, state
                ));
            }
        }
    }

    out
}
