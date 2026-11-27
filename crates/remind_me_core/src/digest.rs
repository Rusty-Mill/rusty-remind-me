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
//! Reminders come from [`crate::reminders::list_reminders`], the exact
//! function `remind_me_list_reminders` calls. That is what makes it
//! structurally impossible for the digest to disagree with it; a second
//! query here would be a second opinion about what "overdue" means.

use crate::db::Result;
use crate::db::Store;
use crate::models::{DigestData, DigestReminder, DigestReview, ReminderWindow};
use crate::vitality::build_vitality_report;

/// Days back that count as "recent" by default.
pub const DEFAULT_SINCE_DAYS: i64 = 7;
/// Cap on the memories listed. `recent_total` carries the true count, so a
/// busy week reads as "20 of 340" rather than silently looking like 20.
pub const MAX_RECENT_MEMORIES: usize = 20;

pub const DIGEST_SINCE_DAYS_MIN: i64 = 1;
pub const DIGEST_SINCE_DAYS_MAX: i64 = 365;

/// Cap on the reminders listed per section.
pub const MAX_DIGEST_REMINDERS: i64 = 10;

/// A memory's content cut to the digest's 120 characters.
fn snippet(memory: &crate::models::Memory) -> String {
    let mut content: String = memory.content.chars().take(120).collect();
    if memory.content.chars().count() > 120 {
        content.push('…');
    }
    content
}

/// Project a memory down to what the digest shows for a reminder.
fn digest_reminder(memory: &crate::models::Memory) -> DigestReminder {
    DigestReminder {
        id: memory.id.clone(),
        remind_at: memory.remind_at.clone(),
        content: snippet(memory),
    }
}

/// Project a decision down to what the digest shows for a due review.
fn digest_review(memory: &crate::models::Memory) -> DigestReview {
    DigestReview {
        id: memory.id.clone(),
        due: crate::review::review_due(memory).unwrap_or("").to_string(),
        content: snippet(memory),
    }
}

/// The newest recent memories no mute covers, at most
/// [`MAX_RECENT_MEMORIES`], and how many muted ones were passed over
/// (card #1485). Mute records themselves are never listed. Reads at most
/// twice the cap, so a heavily muted week can list fewer.
fn unmuted_recent(
    store: &Store<'_>,
    stats: &crate::db::stats::StoreStats<'_>,
    cutoff: &str,
) -> Result<(Vec<crate::models::DigestRecentMemory>, usize)> {
    let mutes = crate::mute::MuteSet::load(store, None, None)?;
    let memories = crate::db::memories::Memories::new(store);
    let mut kept = Vec::new();
    let mut hidden = 0;
    for recent in stats.shareable_since(cutoff, MAX_RECENT_MEMORIES * 2)? {
        if kept.len() == MAX_RECENT_MEMORIES {
            break;
        }
        if recent.category == crate::mute::CATEGORY {
            continue;
        }
        if !mutes.is_empty()
            && memories
                .get_live(&recent.id)?
                .is_some_and(|m| mutes.covers(&m))
        {
            hidden += 1;
            continue;
        }
        kept.push(recent);
    }
    Ok((kept, hidden))
}

/// Assemble the digest's underlying data.
pub fn build_digest(store: &Store<'_>, since_days: i64) -> Result<DigestData> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(since_days)).to_rfc3339();

    let stats = crate::db::stats::StoreStats::new(store);
    let (recent_memories, muted_hidden) = unmuted_recent(store, &stats, &cutoff)?;

    // Counted rather than taken from the capped list, so the cap is visible
    // rather than silently making a busy week look like a quiet one.
    let recent_total = stats.count_shareable_since(&cutoff)?;

    Ok(DigestData {
        generated_at: chrono::Utc::now().to_rfc3339(),
        since_days,
        recent_memories,
        recent_total,
        vitality: build_vitality_report(store)?,
        reminders_upcoming: crate::reminders::list_reminders(
            store,
            ReminderWindow::Upcoming,
            MAX_DIGEST_REMINDERS,
        )?
        .iter()
        .map(digest_reminder)
        .collect(),
        reminders_overdue: crate::reminders::list_reminders(
            store,
            ReminderWindow::Overdue,
            MAX_DIGEST_REMINDERS,
        )?
        .iter()
        .map(digest_reminder)
        .collect(),
        // Shared with the session brief, so the two agree on what is due.
        reviews_due: crate::review::list_due(store, None, MAX_DIGEST_REMINDERS as usize)?
            .iter()
            .map(digest_review)
            .collect(),
        muted_hidden,
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
    if data.muted_hidden > 0 {
        out.push_str(&format!(
            "_{} muted memor{} left out._\n\n",
            data.muted_hidden,
            if data.muted_hidden == 1 { "y" } else { "ies" }
        ));
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

    out.push_str("## Decisions to review\n\n");
    if data.reviews_due.is_empty() {
        out.push_str("_None due._\n\n");
    } else {
        for review in &data.reviews_due {
            out.push_str(&format!(
                "- `{}` due {}: {}\n",
                review.id, review.due, review.content
            ));
        }
        out.push_str(
            "\n_Record each with `remind_me_resolve` and `review`: held, revised or reverted._\n\n",
        );
    }

    out
}
