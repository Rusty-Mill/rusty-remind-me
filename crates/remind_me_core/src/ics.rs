//! RFC 5545 calendar feed generation for the reminders subscription.
//!
//! No iCalendar crate. The read-only subset needed here — one VCALENDAR
//! wrapping a VEVENT per reminder, each with UID/DTSTAMP/DTSTART/SUMMARY/
//! DESCRIPTION — is small enough to write directly and test exhaustively,
//! and this workspace has consistently preferred that to a dependency for
//! something this size.
//!
//! # Two details that decide whether real calendar clients accept the feed
//!
//! Both produce output that looks fine in a text editor and fails in
//! practice, which is why each has its own tests:
//!
//! - **Line folding (§3.1).** A content line may not exceed 75 octets. Longer
//!   ones are split with `CRLF SPACE`, which the reader strips back out.
//!   Apple Calendar in particular rejects unfolded long lines. [`fold_line`]
//!   splits on UTF-8 character boundaries, never mid-sequence, so a reminder
//!   written in any non-ASCII script survives the round trip.
//! - **Text escaping (§3.3.11).** Backslash, comma, semicolon and newlines
//!   are structural in the TEXT value type. Left unescaped, one reminder
//!   containing a comma corrupts *every* VEVENT after it in the document —
//!   the failure is not local to the offending entry.
//!
//! # UIDs are deterministic
//!
//! `{memory_id}-{remind_at}@remind-me`, not a random UUID. A subscribed
//! calendar re-fetches this feed on its own schedule, and a stable UID is
//! what lets it recognise an unchanged reminder as the same event instead of
//! creating a duplicate on every poll. Rescheduling deliberately mints a new
//! UID, because that is a genuinely different occurrence.

use crate::models::Memory;
use chrono::{DateTime, Utc};

/// RFC 5545 PRODID identifying what generated the calendar.
pub const PRODID: &str = "-//remind-me-mcp//Reminders//EN";

/// Truncation length for SUMMARY, a one-line calendar title. DESCRIPTION
/// always carries the full content, so nothing is lost.
pub const SUMMARY_MAX_CHARS: usize = 100;

/// Max octets per physical content line (RFC 5545 §3.1).
const FOLD_LIMIT: usize = 75;

/// Escape a TEXT value per RFC 5545 §3.3.11.
///
/// Backslash goes first, or the backslashes introduced by the later escapes
/// would themselves be escaped. CRLF, bare LF and bare CR all collapse to the
/// two-character sequence `\n`.
pub fn escape_ics_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
        // CRLF first, so a Windows line ending collapses to one `\n` rather
        // than two.
        .replace("\r\n", "\\n")
        .replace(['\n', '\r'], "\\n")
}

/// Fold one logical content line at 75 octets (RFC 5545 §3.1).
///
/// The first physical line carries up to 75 octets. Each continuation line
/// begins with a space that counts against its own budget, so continuations
/// carry at most 74.
pub fn fold_line(line: &str) -> String {
    if line.len() <= FOLD_LIMIT {
        return line.to_string();
    }

    let bytes = line.as_bytes();
    let mut chunks: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut limit = FOLD_LIMIT;

    while start < bytes.len() {
        let mut end = (start + limit).min(bytes.len());
        // Back off out of the middle of a multi-byte character. Splitting
        // there would put half a character on each line and produce two
        // invalid UTF-8 fragments.
        while end > start && end < bytes.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&line[start..end]);
        start = end;
        limit = FOLD_LIMIT - 1;
    }

    chunks.join("\r\n ")
}

/// Render a timestamp as a `Z`-suffixed UTC DATE-TIME.
pub fn format_utc_stamp(when: DateTime<Utc>) -> String {
    when.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Render reminders as an RFC 5545 VCALENDAR document.
///
/// `now` is the DTSTAMP shared by every VEVENT. Taking it as a parameter
/// makes the output byte-identical across calls, which is what lets the tests
/// assert on whole documents.
///
/// A memory with no `remind_at` is skipped rather than treated as an error,
/// so a caller may pass any memory-shaped rows without pre-filtering.
pub fn build_ics(reminders: &[Memory], now: DateTime<Utc>) -> String {
    let stamp = format_utc_stamp(now);
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        format!("PRODID:{}", PRODID),
        "CALSCALE:GREGORIAN".to_string(),
    ];

    for memory in reminders {
        let Some(remind_at) = memory.remind_at.as_deref().filter(|r| !r.is_empty()) else {
            continue;
        };
        let Some(when) = crate::reminders::parse_remind_at(remind_at) else {
            // An unparseable stored timestamp cannot become a DTSTART, and
            // emitting a VEVENT without one would make the whole document
            // invalid rather than just that entry wrong.
            continue;
        };

        let summary: String = if memory.content.chars().count() <= SUMMARY_MAX_CHARS {
            memory.content.clone()
        } else {
            let mut truncated: String =
                memory.content.chars().take(SUMMARY_MAX_CHARS - 1).collect();
            truncated.push('…');
            truncated
        };

        lines.push("BEGIN:VEVENT".to_string());
        lines.push(fold_line(&format!(
            "UID:{}",
            escape_ics_text(&format!("{}-{}@remind-me", memory.id, remind_at))
        )));
        lines.push(format!("DTSTAMP:{}", stamp));
        lines.push(format!("DTSTART:{}", format_utc_stamp(when)));
        lines.push(fold_line(&format!("SUMMARY:{}", escape_ics_text(&summary))));
        lines.push(fold_line(&format!(
            "DESCRIPTION:{}",
            escape_ics_text(&memory.content)
        )));
        lines.push("END:VEVENT".to_string());
    }

    lines.push("END:VCALENDAR".to_string());
    lines.join("\r\n") + "\r\n"
}

// ---------------------------------------------------------------------------
// The feed token
// ---------------------------------------------------------------------------

pub const ICS_TOKEN_ENV: &str = "REMIND_ME_ICS_TOKEN";
pub const ICS_TOKEN_FILE_ENV: &str = "REMIND_ME_ICS_TOKEN_FILE";

/// Default token file, beside the connector token this crate already writes
/// (`~/.remind-me/ics_token`, alongside [`crate::db::DEFAULT_DIR_NAME`]).
fn default_token_file() -> std::path::PathBuf {
    crate::db::resolve_memory_dir_child("ics_token")
}

pub fn token_file_path() -> std::path::PathBuf {
    std::env::var(ICS_TOKEN_FILE_ENV)
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_token_file)
}

/// Resolve the effective feed token, mirroring `remote::resolve_connector_token`:
///
/// 1. [`ICS_TOKEN_ENV`] when set and non-blank.
/// 2. The token persisted at [`token_file_path`].
/// 3. First use: generate one and persist it `0600`.
///
/// **There is deliberately no "disabled" opt-out**, unlike the HTTP API key.
/// The token *is* the URL path, so falling open would publish every reminder
/// to anyone who guessed the route. A token file that can be neither read nor
/// written yields an ephemeral per-process token rather than no token at all —
/// a feed nobody can subscribe to beats a feed anybody can read.
///
/// Rotation is by deleting the file, which is the reference's whole story
/// here: there is no revocation list and no second valid token, so rotating
/// means re-pointing every subscribed calendar.
pub fn resolve_ics_token() -> String {
    if let Ok(from_env) = std::env::var(ICS_TOKEN_ENV) {
        let from_env = from_env.trim().to_string();
        if !from_env.is_empty() {
            return from_env;
        }
    }

    let file = token_file_path();
    if let Ok(existing) = std::fs::read_to_string(&file) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }

    let token = crate::remote::generate_token();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&file, format!("{token}\n")).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
    }
    token
}

/// The feed's path for a given token.
pub fn feed_path(token: &str) -> String {
    format!("/api/reminders/{}.ics", token)
}

/// [`build_ics`] stamped with the current time — what a live feed wants, and
/// what keeps `chrono` from leaking into callers that have no other use for it.
pub fn build_ics_now(reminders: &[Memory]) -> String {
    build_ics(reminders, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_token_file_uses_the_hyphenated_data_directory() {
        // Regression: this used to hardcode `.remind_me` (underscored), a
        // directory nothing else in this port reads or writes -- see
        // `remote::default_token_file`'s doc for the same fix applied there.
        let path = default_token_file();
        assert_eq!(path.file_name().unwrap(), "ics_token");
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            crate::db::DEFAULT_DIR_NAME
        );
    }
}
