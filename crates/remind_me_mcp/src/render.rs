//! Markdown renderings for tools that previously had only a JSON response (#206).
//!
//! # Why these exist, and why JSON stays the default
//!
//! The reference returns Markdown from these thirteen tools and offers no JSON
//! at all — ten of them have no `response_format` field, and four take no
//! parameters whatsoever. This port returned JSON and offered no Markdown. Both
//! are half a surface.
//!
//! Adding Markdown here rather than replacing JSON is deliberate: JSON was
//! already this port's observable behaviour, so **`response_format` defaults to
//! `json` for these tools and Markdown is opt-in**. Every existing caller keeps
//! working unchanged, and the capability gap against the reference closes.
//!
//! That does mean the *default* output still differs from the reference's. The
//! alternative — flipping the default to Markdown for parity — would break
//! every current caller to imitate a limitation, which is a bad trade.
//!
//! `remind_me_history` is deliberately untouched: it already offers both and
//! already defaults to Markdown, so changing its default is the one place where
//! "JSON by default" would be a regression rather than a no-op.
//!
//! # These are presentation only
//!
//! Nothing here reads the database or changes a value. Each function takes an
//! already-computed response and formats it, so a rendering bug can misreport
//! but cannot corrupt.

use remind_me_core::expansion::{MemorySearchResponse, RelatedMemory};
use remind_me_core::models::{
    CaptureResult, Memory, RevertOutcome, SavedSearch, SetReminderOutcome,
};
use remind_me_core::updater::UpdateStatus;
use remind_me_core::vectors::ReindexResult;
use remind_me_core::wiki::WikiPage;
use remind_me_core::wiki_fs::WikiCompile;

/// Truncate for a one-line summary, on a character boundary.
fn preview(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

pub fn memory_stored(memory: &Memory) -> String {
    format!(
        "✓ Memory stored with id `{}` in category '{}'.",
        memory.id, memory.category
    )
}

pub fn memory_updated(memory: &Memory) -> String {
    format!(
        "✓ Memory `{}` updated.\n\n{}",
        memory.id,
        preview(&memory.content, 200)
    )
}

pub fn revert_outcome(outcome: &RevertOutcome) -> String {
    // Four variants, four different things to tell a caller. "No change" in
    // particular is a success that did nothing, which a generic ✓ would hide.
    match outcome {
        RevertOutcome::Reverted { revision_id } => {
            format!("✓ Reverted to revision {revision_id}.")
        }
        RevertOutcome::NoChange => {
            "No change — the memory already holds that revision's values.".to_string()
        }
        RevertOutcome::MemoryNotFound => "Memory not found.".to_string(),
        RevertOutcome::RevisionNotFound => "Revision not found.".to_string(),
    }
}

pub fn set_reminder_outcome(outcome: &SetReminderOutcome) -> String {
    // Every variant gets its own line rather than a generic "done": clearing a
    // reminder, rejecting an unparseable time and setting one are three
    // different things a caller may need to react to differently.
    match outcome {
        SetReminderOutcome::Set {
            memory_id,
            remind_at,
        } => format!("✓ Reminder set on `{memory_id}` for {remind_at}."),
        SetReminderOutcome::Cleared { memory_id } => {
            format!("✓ Reminder cleared on `{memory_id}`.")
        }
        SetReminderOutcome::NotFound { memory_id } => {
            format!("Memory `{memory_id}` not found.")
        }
        SetReminderOutcome::Rejected { reason } => format!("Reminder rejected: {reason}"),
    }
}

pub fn saved_search(search: &SavedSearch) -> String {
    format!(
        "✓ Saved search '{}' stored for query `{}`.",
        search.name, search.query
    )
}

pub fn saved_search_list(searches: &[SavedSearch]) -> String {
    if searches.is_empty() {
        return "_No saved searches._".to_string();
    }
    let mut out = format!("**{} saved search(es)**\n", searches.len());
    for s in searches {
        out.push_str(&format!("\n- **{}** — `{}`", s.name, s.query));
    }
    out
}

pub fn reindex_result(result: &ReindexResult) -> String {
    let mut out = format!(
        "✓ Reindexed: {} missing, {} embedded, {} chunks created.",
        result.missing, result.embedded, result.chunks_created
    );
    if result.degraded {
        // Surfaced rather than folded into the counts: a degraded run looks
        // like a successful one from the numbers alone.
        out.push_str("\n\n**Degraded** — some embeddings could not be produced.");
    }
    out
}

pub fn update_status(status: &UpdateStatus) -> String {
    if let Some(error) = &status.error {
        return format!("Could not check for updates: {error}");
    }
    if status.update_available {
        format!(
            "**Update available** — {} commit(s) behind.\n\ninstalled {} ({} → {})",
            status.commits_behind,
            status.installed_version,
            status.local_commit,
            status.remote_commit
        )
    } else {
        format!(
            "Up to date at {} ({}).",
            status.installed_version, status.local_commit
        )
    }
}

pub fn capture_result(result: &CaptureResult) -> String {
    let mut out = format!(
        "✓ Captured `{}` — \"{}\" in category '{}'.",
        result.capture_id, result.title, result.category
    );
    if !result.tags.is_empty() {
        out.push_str(&format!("\n\ntags: {}", result.tags.join(", ")));
    }
    out
}

pub fn wiki_compile(outcome: &WikiCompile) -> String {
    // The three variants are three phases, not degrees of success: a brief is
    // work still to do, integrated is work finished, noop is nothing pending.
    match outcome {
        WikiCompile::Brief {
            pending,
            watermark,
            brief,
        } => format!("**{pending} pending** (watermark {watermark})\n\n{brief}"),
        WikiCompile::Integrated {
            sources_marked,
            watermark,
        } => format!("✓ Integrated {sources_marked} source(s); watermark now {watermark}."),
        WikiCompile::Noop { reason, watermark } => {
            format!("Nothing to compile: {reason} (watermark {watermark})")
        }
    }
}

pub fn wiki_page(page: &WikiPage) -> String {
    format!("# {}\n\n{}", page.title, page.content)
}

/// Renders the **enriched** status value, not the bare `ServerStatus`.
///
/// The dispatch layer overwrites `dashboard`, `sync`, `webhook` and `remote`
/// with live state the core crate cannot see on its own (a separate
/// dashboard process's PID file, the sync worker's in-memory counters, the
/// webhook listener, the remote connector's env/token state). Only `mcp` and
/// `embeddings` keep the core crate's tagged `SubsystemStatus` shape
/// (`{"state": "active"}` / `{"state": "not_implemented", ...}`) — the other
/// four are untagged structs (`DashboardStatus`/`SyncWorkerStatus`/
/// `WebhookStatus`/`RemoteStatus`) with no `state` field at all. Reading all
/// six the same generic way rendered `?` for those four regardless of their
/// real state, even while `remind_me_server_status`'s own JSON output was
/// correct — [`subsystem_line`] renders each shape on its own terms instead.
fn subsystem_line(key: &str, v: &serde_json::Value) -> String {
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);

    match key {
        "dashboard" => {
            if b("running") {
                format!("active ({})", s("url").unwrap_or_default())
            } else {
                "not running".to_string()
            }
        }
        "sync" => {
            if !b("enabled") {
                return "disabled".to_string();
            }
            match s("last_error") {
                Some(err) => format!("error: {err} (after {} cycles)", n("cycles")),
                None => format!("active ({} cycles)", n("cycles")),
            }
        }
        "webhook" => {
            if !b("enabled") {
                return "disabled".to_string();
            }
            if !b("running") {
                return match s("start_error") {
                    Some(err) => format!("enabled, not listening: {err}"),
                    None => "enabled, not listening".to_string(),
                };
            }
            format!(
                "active ({}:{}, {} ingested)",
                s("bind").unwrap_or_default(),
                n("port"),
                n("requests_ingested")
            )
        }
        "remote" => {
            if !b("enabled") {
                "disabled".to_string()
            } else {
                format!("active ({}:{})", s("host").unwrap_or_default(), n("port"))
            }
        }
        // `mcp`/`embeddings`: the tagged `SubsystemStatus` shape.
        _ => v
            .get("state")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string(),
    }
}

pub fn server_status(report: &serde_json::Value) -> String {
    let s = |k: &str| -> String {
        report
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let n = |k: &str| -> i64 { report.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };

    let mut out = format!("**rusty-remind-me {}**\n", s("version"));
    out.push_str(&format!(
        "\n- database: {}",
        report
            .get("database_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(in memory, no file)")
    ));
    out.push_str(&format!("\n- memories: {}", n("memory_count")));
    let current = report
        .get("schema_current")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    out.push_str(&format!(
        "\n- schema: v{} (expected v{}){}",
        n("schema_version"),
        n("expected_schema_version"),
        if current { "" } else { " — **MISMATCH**" }
    ));
    out.push_str(&format!("\n- backups: {}", n("backup_count")));
    for key in [
        "mcp",
        "dashboard",
        "embeddings",
        "sync",
        "webhook",
        "remote",
    ] {
        let line = report
            .get(key)
            .map(|v| subsystem_line(key, v))
            .unwrap_or_else(|| "?".to_string());
        out.push_str(&format!("\n- {key}: {line}"));
    }
    let watcher = report.get("watcher");
    let flag = |k: &str| -> bool {
        watcher
            .and_then(|w| w.get(k))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    out.push_str(&format!(
        "\n- watcher: {}",
        if flag("running") {
            "running"
        } else if flag("enabled") {
            "configured, not running"
        } else {
            "disabled"
        }
    ));
    out
}

// ---------------------------------------------------------------------------
// Tools that mirror a reference model (#224)
//
// Unlike everything above, these are not additive. The reference already
// returns Markdown from `remind_me_wiki_list` by default and offers JSON as the
// opt-in; this port had it backwards. So these two renderings reproduce the
// reference's layout rather than inventing one -- the text is what a model
// reads, and a different shape is a different prompt.
// ---------------------------------------------------------------------------

/// The wiki index, as `tools/wiki.py:130-134` renders it.
pub fn wiki_page_list(pages: &[WikiPage]) -> String {
    if pages.is_empty() {
        // Verbatim from the reference, pointer to `wiki_compile` included: an
        // empty index is far more often "nothing synthesised yet" than "nothing
        // to synthesise", and the message is what says which.
        return "_The wiki is empty._ Synthesise pages from raw memories with \
                `remind_me_wiki_compile`."
            .to_string();
    }
    let mut lines = vec![format!("## Wiki — {} page(s)", pages.len()), String::new()];
    for p in pages {
        // `[[title]]` wikilinks, and the summary omitted entirely when blank
        // rather than rendered as a trailing em dash with nothing after it.
        let summary = if p.summary.is_empty() {
            String::new()
        } else {
            format!(" — {}", p.summary)
        };
        lines.push(format!("- [[{}]]{}", p.title, summary));
    }
    lines.join("\n")
}

/// The vault vitality report, as `tools/lifecycle.py:69-90` renders it.
pub fn vitality_report(report: &remind_me_core::vitality::VitalityReport) -> String {
    let mut lines = vec![
        "## Vault Vitality Report".to_string(),
        String::new(),
        format!("**Total memories:** {}", report.total_memories),
        format!("**Active:** {}", report.active_count),
        format!("**Dormant:** {}", report.dormant_count),
        format!("**Vault health:** {}", report.vault_health_score),
        // `:.2f` in the reference. Two places, not Rust's default float
        // formatting, which would print `0.8333333333333334`.
        format!("**Average vitality:** {:.2}", report.average_vitality),
        String::new(),
        "### Vitality Distribution".to_string(),
        String::new(),
    ];
    for (label, count) in &report.vitality_buckets {
        // The bar caps at 40 so one enormous bucket cannot produce a line
        // thousands of characters wide -- the reference's `min(count, 40)`.
        let bar = "#".repeat((*count).min(40));
        lines.push(format!("  {label}: {bar} ({count})"));
    }
    lines.push(String::new());
    lines.push("### Memory Type Distribution".to_string());
    lines.push(String::new());
    // `decay_distribution` is a BTreeMap, so iteration is already sorted --
    // matching the reference's explicit `sorted(...)`.
    for (kind, count) in &report.decay_distribution {
        lines.push(format!("- **{kind}**: {count}"));
    }
    lines.join("\n")
}

/// A search response, as close to `tools/search.py:936-992` as this port's data
/// allows.
///
/// # What is faithful, and what is missing
///
/// Reproduced exactly: the results themselves (`_fmt_memory_md`, via
/// [`remind_me_core::reminders::render_memories_markdown`]), the budget line in
/// both its trimmed and untrimmed forms, the `_No memories found._` empty case,
/// the `\n---\n` joiner, and the three expansion sections.
///
/// **Deliberately absent, because this port does not track the inputs:**
///
/// - the per-hit method badge (`⚡ hybrid` / `🔮 semantic` / `🔤 keyword`) and
///   `distance: N`, which need a per-result search-method tag,
/// - the `_Tiers: N keyword, N semantic, N hybrid | N dormant excluded_`
///   footer, which needs a tier breakdown and a dormant-exclusion count,
/// - the `verbose` debug-signal line, which needs per-result rank positions.
///
/// None of those exist anywhere in `remind_me_core`. Adding them is search
/// pipeline work rather than rendering work, so this deliberately renders less
/// than the reference rather than inventing substitutes that would look right
/// and mean something else.
/// One line saying the deadline changed what this search did, or `None` when
/// it ran in full (#257).
///
/// Names the stages rather than saying "timed out", because the two skips mean
/// different things to a reader: no semantic stage means these are keyword
/// matches only — so "there is nothing about this topic" is *not* a safe
/// inference — while no rerank means the ordering is RRF's rather than the
/// cross-encoder's, which is worse but not misleading.
fn degradation_note(res: &MemorySearchResponse) -> Option<String> {
    if !res.timing.degraded() {
        return None;
    }
    let limit = res
        .timing
        .deadline_ms
        .map(|ms| format!("{ms}ms deadline"))
        .unwrap_or_else(|| "deadline".to_string());
    Some(format!(
        "⏱ _Degraded: skipped {} after {}ms ({}). Results are incomplete rather than exhaustive._\n",
        res.timing.skipped.join(" and "),
        res.timing.elapsed_ms,
        limit
    ))
}

pub fn search_response(res: &MemorySearchResponse) -> String {
    let bootstrap = res.bootstrap.as_ref().filter(|b| !b.is_empty());

    // A bootstrap without hits is still an answer. The persona was assembled
    // regardless of the query, so "nothing matched" and "there is nothing to
    // tell you" stopped being the same statement the moment #255 landed —
    // returning the bare empty case here would throw away context the caller
    // explicitly asked for and already paid budget for.
    // A search that ran out of time and found nothing is not the same as one
    // that looked everywhere and found nothing, and the bare empty case reads
    // as the second. Saying so matters most here: with results on screen a
    // caller might notice the ranking is off, but "no memories" looks
    // authoritative and is the answer they are most likely to act on.
    if res.memories.is_empty() && bootstrap.is_none() {
        return match degradation_note(res) {
            Some(note) => format!("_No memories found._\n\n{note}"),
            None => "_No memories found._".to_string(),
        };
    }

    let mut parts: Vec<String> = Vec::new();

    // Ahead of the results, and outside the ranked list: this is context for
    // the query rather than an answer to it, and interleaving it with the hits
    // would misreport durable statements as things that matched.
    if let Some(b) = bootstrap {
        let mut section = String::from("**Persona** (durable context, not query matches):\n");
        for statement in &b.statements {
            section.push_str(&format!("- {}\n", statement.content.trim()));
        }
        if b.omitted > 0 {
            section.push_str(&format!(
                "\n_{} further statement{} omitted to stay inside the bootstrap reserve (~{} tokens)._\n",
                b.omitted,
                if b.omitted == 1 { "" } else { "s" },
                b.tokens_used
            ));
        }
        parts.push(section);
    }

    if res.memories.is_empty() {
        parts.push("_No memories matched the query._".to_string());
        if let Some(note) = degradation_note(res) {
            parts.push(note);
        }
        return parts.join("\n---\n");
    }
    // The reference names the retrieval method here. Without a per-result
    // method tag this port cannot say which ran, and guessing "hybrid" would be
    // a claim rather than a report -- so the count is stated and the method is
    // not.
    parts.push(format!("**{} results**", res.returned));
    // Ahead of the budget envelope, because it qualifies the result set more
    // strongly than the trim does: trimming drops results that were found,
    // whereas a skipped stage means results that were never looked for.
    if let Some(note) = degradation_note(res) {
        parts.push(note);
    }
    if res.trimmed > 0 {
        parts.push(format!(
            "_{} of {} candidates (trimmed {}, ~{}/{} tokens)_\n",
            res.returned, res.total_candidates, res.trimmed, res.tokens_used, res.budget
        ));
    } else {
        parts.push(format!(
            "_~{} tokens used (budget: {})_\n",
            res.tokens_used, res.budget
        ));
    }

    let memories: Vec<_> = res.memories.iter().map(|r| r.memory.clone()).collect();
    parts.push(remind_me_core::reminders::render_memories_markdown(
        &memories,
    ));

    if let Some(related) = res.related_via_entities.as_ref().filter(|r| !r.is_empty()) {
        parts.push(expansion_section(
            &format!(
                "**Related via entities** (1-hop expansion, max {}):",
                remind_me_core::expansion::EXPANSION_CAP
            ),
            related,
            true,
        ));
    }
    if let Some(related) = res.related_via_neighbors.as_ref().filter(|r| !r.is_empty()) {
        parts.push(expansion_section(
            "**Related via document neighbors**:",
            related,
            false,
        ));
    }
    if let Some(related) = res
        .related_via_co_retrieval
        .as_ref()
        .filter(|r| !r.is_empty())
    {
        parts.push(expansion_section(
            "**Related via co-retrieval**:",
            related,
            false,
        ));
    }

    parts.join("\n---\n")
}

/// One expansion block, matching `_fmt_expansion_md`'s per-item shape.
fn expansion_section(heading: &str, related: &[RelatedMemory], via_entities: bool) -> String {
    let mut lines = vec![heading.to_string()];
    for item in related {
        // `" ".join(str(...).split())` in the reference: collapse all runs of
        // whitespace, including newlines, so a multi-line memory does not break
        // the list item across lines.
        let snippet: String = item
            .content_snippet
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // Truncated at 120 *characters*, not bytes -- slicing a String by byte
        // index would panic mid-codepoint on any non-ASCII memory.
        let snippet = if snippet.chars().count() > 120 {
            format!("{}…", snippet.chars().take(120).collect::<String>())
        } else {
            snippet
        };
        if via_entities && !item.via_entities.is_empty() {
            lines.push(format!(
                "- `{}` {} _(via: {})_",
                item.id,
                snippet,
                item.via_entities.join(", ")
            ));
        } else {
            lines.push(format!("- `{}` {}", item.id, snippet));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod bootstrap_rendering {
    use super::*;
    use remind_me_core::promotion::Bootstrap;
    use remind_me_core::PersonaStatement;

    fn statement(content: &str) -> PersonaStatement {
        PersonaStatement {
            id: "mem_p".to_string(),
            content: content.to_string(),
            vitality: 1.0,
            created_at: "2026-08-10T00:00:00Z".to_string(),
            surviving_sources: 1,
        }
    }

    fn response(bootstrap: Option<Bootstrap>) -> MemorySearchResponse {
        MemorySearchResponse {
            memories: Vec::new(),
            total_candidates: 0,
            returned: 0,
            trimmed: 0,
            tokens_used: 0,
            budget: 800,
            timing: Default::default(),
            bootstrap,
            related_via_entities: None,
            related_via_neighbors: None,
            related_via_co_retrieval: None,
        }
    }

    #[test]
    fn nothing_at_all_still_renders_the_reference_empty_case() {
        assert_eq!(search_response(&response(None)), "_No memories found._");
    }

    #[test]
    fn an_empty_bootstrap_is_not_treated_as_a_bootstrap() {
        // Asking for one and getting nothing must read the same as not asking.
        // Otherwise every search on a store with no persona yet would carry an
        // empty heading.
        assert_eq!(
            search_response(&response(Some(Bootstrap::default()))),
            "_No memories found._"
        );
    }

    #[test]
    fn a_bootstrap_without_hits_is_still_an_answer() {
        let rendered = search_response(&response(Some(Bootstrap {
            statements: vec![statement("Ships small changes.")],
            tokens_used: 5,
            omitted: 0,
        })));

        assert!(rendered.contains("Ships small changes."));
        // The distinction the change exists for: the query matched nothing,
        // but the caller was not told there is nothing to tell them.
        assert!(rendered.contains("_No memories matched the query._"));
        assert!(!rendered.contains("_No memories found._"));
        assert!(
            rendered.find("Ships small changes.") < rendered.find("_No memories matched"),
            "context comes before the report of what matched"
        );
    }

    #[test]
    fn a_trimmed_bootstrap_says_how_much_it_withheld() {
        let rendered = search_response(&response(Some(Bootstrap {
            statements: vec![statement("Ships small changes.")],
            tokens_used: 5,
            omitted: 3,
        })));
        // A count, not a boolean -- same reasoning as the trim envelope.
        assert!(rendered.contains("3 further statements omitted"));
    }

    #[test]
    fn one_withheld_statement_is_not_pluralised() {
        let rendered = search_response(&response(Some(Bootstrap {
            statements: vec![statement("Ships small changes.")],
            tokens_used: 5,
            omitted: 1,
        })));
        assert!(rendered.contains("1 further statement omitted"));
    }
}

#[cfg(test)]
mod deadline_rendering {
    use super::*;
    use remind_me_core::retrieval::SearchTiming;

    fn response(timing: SearchTiming, hits: usize) -> MemorySearchResponse {
        MemorySearchResponse {
            memories: Vec::new(),
            total_candidates: hits,
            returned: hits,
            trimmed: 0,
            tokens_used: 0,
            budget: 800,
            timing,
            bootstrap: None,
            related_via_entities: None,
            related_via_neighbors: None,
            related_via_co_retrieval: None,
        }
    }

    fn degraded() -> SearchTiming {
        SearchTiming {
            elapsed_ms: 1200,
            deadline_ms: Some(1000),
            skipped: vec!["semantic".to_string()],
        }
    }

    #[test]
    fn a_clean_run_says_nothing_about_timing() {
        let rendered = search_response(&response(SearchTiming::default(), 0));
        assert_eq!(rendered, "_No memories found._");
        assert!(!rendered.contains("Degraded"));
    }

    #[test]
    fn finding_nothing_after_a_timeout_does_not_read_as_finding_nothing() {
        let rendered = search_response(&response(degraded(), 0));
        // The dangerous case: "no memories" looks authoritative, and a caller
        // is most likely to act on it.
        assert!(rendered.contains("_No memories found._"));
        assert!(rendered.contains("Degraded"));
        assert!(rendered.contains("semantic"));
        assert!(rendered.contains("1000ms deadline"));
    }

    #[test]
    fn the_note_names_the_stages_rather_than_saying_timed_out() {
        let rendered = search_response(&response(
            SearchTiming {
                elapsed_ms: 2000,
                deadline_ms: Some(500),
                skipped: vec!["semantic".to_string(), "rerank".to_string()],
            },
            0,
        ));
        // Which stage was lost changes what the reader may infer: no semantic
        // means "nothing about this topic" is not a safe conclusion, whereas
        // no rerank only means the order is worse.
        assert!(rendered.contains("semantic and rerank"));
    }
}
