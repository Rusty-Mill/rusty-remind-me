//! Memory-store statistics, shared by the MCP tool, the MCP resource, and the
//! HTTP route so the three cannot drift apart.

use crate::db::stats::{GroupBy, StorageInfo, StoreStats};
use rusqlite::{Connection, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A recently created memory, trimmed for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMemory {
    pub id: String,
    pub category: String,
    /// First 80 characters of the content, matching the reference.
    pub preview: String,
    pub created_at: String,
}

/// Snapshot of the memory store.
///
/// Field names mirror the reference's `memory_stats` payload so a client can
/// read either server's response unchanged.
///
/// `categories` and `sources` are keyed maps, and this crate emits them in
/// alphabetical order where the reference emits them count-descending. JSON
/// objects are unordered by specification, so a consumer that cares about
/// ranking must sort by value itself against either implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub total_memories: i64,
    pub total_imports: i64,
    pub categories: BTreeMap<String, i64>,
    pub sources: BTreeMap<String, i64>,
    pub recent: Vec<RecentMemory>,
    pub db_path: String,
    pub db_size_mb: f64,
}

/// Dashboard-facing statistics — `GET /api/stats`, matching the reference's
/// own dashboard-only route (`api.py:531-562`), not `remind_me_stats`.
///
/// The reference itself uses two different response shapes for the same
/// numbers depending on the caller: the MCP tool returns [`Stats`]'s
/// `total_memories`/`total_imports`/`recent`, while its dashboard route
/// returns `total`/`imports`/`tags` and omits `recent` entirely. This
/// struct exists because the vendored dashboard JSX (`crates/remind_me_api`)
/// reads the second shape — `stats.total` and `stats.tags` specifically —
/// and a server that answers with [`Stats`] instead leaves every count in
/// the dashboard's header, sidebar, and "Unique Tags" card reading `0` via
/// its `||0` fallbacks, silently, with no error to notice.
///
/// `tags` is alphabetical here (via `BTreeMap`), same as `categories` and
/// `sources` above — the dashboard's own "Top Tags" chart re-sorts by count
/// client-side (`App.jsx`: `Object.entries(stats.tags).sort((a,b)=>b[1]-a[1])`),
/// so wire order is not load-bearing for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    pub total: i64,
    pub imports: i64,
    pub categories: BTreeMap<String, i64>,
    pub sources: BTreeMap<String, i64>,
    pub tags: BTreeMap<String, i64>,
    pub db_path: String,
    pub db_size_mb: f64,
}

/// How many recent memories the reference includes.
const RECENT_LIMIT: usize = 5;

/// Size of the main database in MiB, rounded to two decimals.
///
/// Derived from SQLite's own page accounting rather than a filesystem `stat`,
/// so it is correct for an in-memory database too — where there is no file to
/// stat and the reference would report 0.
fn size_mb(info: &StorageInfo) -> f64 {
    (info.size_bytes as f64 / 1_048_576.0 * 100.0).round() / 100.0
}

/// Path of the main database, or an empty string for an in-memory one.
fn path_text(info: &StorageInfo) -> String {
    info.path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Collect statistics for the whole memory store.
///
/// Every query is fallible and errors propagate. The call sites this replaces
/// each used `.unwrap_or(0)`, which reported an empty store when the database
/// was actually unreadable — `CONTRIBUTING.md` §2 forbids swallowing failures
/// that way.
pub fn collect(conn: &Connection) -> Result<Stats> {
    let stats = StoreStats::new(conn);
    let info = stats.storage_info()?;
    Ok(Stats {
        total_memories: stats.live_memories()?,
        total_imports: stats.imports()?,
        categories: stats.count_by(GroupBy::Category)?,
        sources: stats.count_by(GroupBy::Source)?,
        recent: stats.recent(RECENT_LIMIT as i64)?,
        db_path: path_text(&info),
        db_size_mb: size_mb(&info),
    })
}

/// Collect statistics in the dashboard's own shape ([`DashboardStats`]),
/// not the MCP tool's ([`Stats`]) — see that struct's doc for why the two
/// differ. Reuses the same repository queries and [`path_text`]/[`size_mb`] rather than
/// re-deriving them, so the counts cannot disagree with [`collect`]'s.
pub fn collect_dashboard(conn: &Connection) -> Result<DashboardStats> {
    let stats = StoreStats::new(conn);
    let info = stats.storage_info()?;
    Ok(DashboardStats {
        total: stats.live_memories()?,
        imports: stats.imports()?,
        categories: stats.count_by(GroupBy::Category)?,
        sources: stats.count_by(GroupBy::Source)?,
        // Via the normalized `memory_tags` index rather than parsing every
        // row's JSON `tags` column (the reference's own approach): every write
        // keeps the two in step, and it avoids a `json_each` scan.
        tags: stats.count_by_tag()?,
        db_path: path_text(&info),
        db_size_mb: size_mb(&info),
    })
}

/// Render the stats snapshot the way the reference's `remind_me_stats`
/// markdown branch does (`tools/admin.py:457`).
///
/// Section order and headings are the reference's, not this crate's
/// preference: the markdown is what a model reads back, so a reordered or
/// renamed section is a different prompt even when the numbers match.
pub fn render_markdown(stats: &Stats) -> String {
    let mut lines = vec![
        "## Memory Store Statistics".to_string(),
        String::new(),
        format!("**Total memories:** {}", stats.total_memories),
        format!("**Total imports:** {}", stats.total_imports),
        format!(
            "**Database:** `{}` ({} MB)",
            stats.db_path, stats.db_size_mb
        ),
        String::new(),
        "### Categories".to_string(),
    ];
    for (category, count) in &stats.categories {
        lines.push(format!("- **{}**: {}", category, count));
    }
    lines.push(String::new());
    lines.push("### Sources".to_string());
    for (source, count) in &stats.sources {
        lines.push(format!("- **{}**: {}", source, count));
    }
    lines.push(String::new());
    lines.push("### Recent Memories".to_string());
    for recent in &stats.recent {
        lines.push(format!(
            "- `{}` [{}] {}…",
            recent.id, recent.category, recent.preview
        ));
    }
    lines.join("\n")
}
