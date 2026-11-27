//! The refinement ladder: capture → fact → scenario → persona.
//!
//! # What was missing
//!
//! The rungs already existed; the ladder did not. `capture.rs` holds raw
//! dialog, `remind_me_decompose` turns one into atomic facts, `wiki.rs`
//! compiles topic pages and `consolidation.rs` merges near-duplicates. But
//! every promotion was agent-initiated and one-shot. Nothing walked the store
//! asking which captures were never decomposed, which facts had accumulated
//! enough to deserve a scenario, or which scenarios were stable enough to say
//! something durable about the user. `UndecomposedCapture` existed — the
//! backlog was visible and simply never worked.
//!
//! This module is the missing walk. It does not distil anything itself: it
//! reports what is *ready* to move up ([`promotion_candidates`]) and accepts
//! the distillation back ([`promote`]), which is how `remind_me_decompose`
//! already works. No LLM is called from here.
//!
//! # Why rung 1 has candidates but no promotion
//!
//! `capture → fact` promotion **is** `remind_me_decompose`, which already
//! exists, already links `source_capture_id`, already applies entity mentions
//! and already supersedes contradicted facts. A second write path to the same
//! rung would be two implementations of one operation, and they would drift.
//! So [`promotion_candidates`] unifies the backlog view across all three rungs
//! and [`promote`] refuses rung 1, naming the tool that does it.
//!
//! # Provenance is not optional
//!
//! Every promoted artifact records which memories it came from, in the
//! target-only `promotions` table. Without it a persona statement is
//! unfalsifiable — you cannot ask what it was derived from, so you cannot tell
//! whether it is still true. The table is indexed both ways, so
//! [`provenance`] answers "what did this come from" and "what was built on
//! this" at the same cost.
//!
//! That is also what makes **demotion** automatic. [`persona`] excludes any
//! statement whose sources have all been superseded or deleted, so a fact
//! contradicted through `supersede_contradicting_facts` silently withdraws the
//! persona built on it, with no background job and no second opinion about
//! what "still true" means.
//!
//! `promotions` is its own table on the engine (`db::engine::promotions`),
//! keyed by the promoted and source ids.

use crate::db::memories::{Memories, NewMemory};
use crate::db::promotions::{Promotions, StatementRow};
use crate::db::Result as SqlResult;
use crate::db::Store;
use crate::models::{
    Memory, PersonaStatement, PromoteInput, PromotionCandidate, PromotionResult, Provenance, Rung,
    FACT_CATEGORY, PERSONA_CATEGORY, PROMOTION_LIMIT_MAX, PROMOTION_LIMIT_MIN, SCENARIO_CATEGORY,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;

/// `source` promoted artifacts are stored under.
pub const PROMOTION_SOURCE: &str = "promotion";

/// Fewest facts sharing an entity before a scenario is worth proposing.
///
/// Two facts about the same entity is a coincidence; the threshold is what
/// stops the candidate list being one entry per entity in the store from the
/// moment anything is decomposed.
pub const MIN_SCENARIO_FACTS: usize = 3;

/// Fewest origins those facts must come from: two sessions, or two facts
/// written directly. One long session repeating itself is not a pattern
/// (ADR-0029).
pub const MIN_SCENARIO_SOURCES: usize = 2;

/// Vitality a scenario must still hold to be persona material.
///
/// The persona rung is meant to be *stable*, so the gate is the decay model
/// already in use rather than a new stability counter: a scenario that has not
/// been touched since it was written falls below this on its own.
pub const PERSONA_VITALITY_FLOOR: f64 = 0.5;

/// Longest snippet carried in a candidate listing.
const SNIPPET_CHARS: usize = 200;

/// Why a promotion could not be made.
#[derive(Debug)]
pub enum PromotionError {
    Db(crate::db::StoreError),
    /// Rung 1 is `remind_me_decompose`'s job.
    UseDecompose,
    /// A promotion with no sources has no provenance, so it could never be
    /// checked or demoted.
    NoSources,
    /// A named source does not exist, or is deleted/superseded.
    UnusableSource(String),
    /// A sensitive memory cannot become part of a persona.
    SensitiveSource(String),
    /// The distillation itself was empty.
    EmptyContent,
    /// This exact source set already has a live promotion at this rung.
    DuplicateSources(String),
    /// `refreshes` names no live promoted memory at the rung promoted to.
    UnusableRefresh(String),
    /// `refreshes` names a line a person pinned (pack D102).
    PinnedRefresh(String),
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{}", e),
            Self::UseDecompose => write!(
                f,
                "capture_to_fact promotion is remind_me_decompose — it already links \
                 source_capture_id, applies entity mentions and supersedes contradicted \
                 facts. Use that tool; this one covers the rungs above it."
            ),
            Self::NoSources => write!(
                f,
                "a promotion needs at least one source memory: without provenance the \
                 result cannot be checked against what it came from, or demoted when \
                 that changes"
            ),
            Self::UnusableSource(id) => write!(
                f,
                "source {:?} does not exist, or has been deleted or superseded",
                id
            ),
            Self::SensitiveSource(id) => write!(
                f,
                "source {:?} is marked sensitive, so it cannot be promoted into a \
                 persona — a persona is an ambient surface, like a digest",
                id
            ),
            Self::EmptyContent => write!(f, "the promoted content is empty"),
            Self::DuplicateSources(id) => write!(
                f,
                "this exact set of sources already has a live promotion, {:?} — \
                 promoting it again would mint a second, independent memory from the \
                 same evidence rather than the one that already exists",
                id
            ),
            Self::UnusableRefresh(id) => write!(
                f,
                "refreshes {:?} names no live promoted memory at the rung promoted to, \
                 so there is nothing for this promotion to replace",
                id
            ),
            Self::PinnedRefresh(id) => write!(
                f,
                "refreshes {:?} is a pinned persona line; unpin it with remind_me_update \
                 (metadata.pinned false) before replacing it",
                id
            ),
        }
    }
}

impl std::error::Error for PromotionError {}

impl From<crate::db::StoreError> for PromotionError {
    fn from(e: crate::db::StoreError) -> Self {
        Self::Db(e)
    }
}

fn snippet(content: &str) -> String {
    content.chars().take(SNIPPET_CHARS).collect()
}

/// Captures nothing has decomposed yet.
///
/// The predicate is `capture::undecomposed_batch`'s, deliberately: one
/// definition of "not yet decomposed", so this listing and that tool cannot
/// disagree about whether a capture is done.
fn capture_candidates(store: &Store<'_>, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let rows = Promotions::new(store).undecomposed_dialogs(limit)?;
    Ok(rows
        .into_iter()
        .map(|row| PromotionCandidate {
            rung: Rung::CaptureToFact,
            source_ids: vec![row.id],
            snippet: snippet(&row.content),
            reason: "captured but never decomposed into facts".to_string(),
            grouped_by: None,
            refresh_of: None,
            new_evidence: Vec::new(),
        })
        .collect())
}

/// Facts clustered by a shared entity, where no scenario has been built from
/// them yet.
///
/// Grouping by entity rather than by embedding similarity on purpose:
/// `consolidation.rs` already clusters on cosine distance to find things that
/// are *the same*, and this rung wants things that are *related but distinct*.
/// The entity graph is the existing structure that expresses that.
fn scenario_candidates(store: &Store<'_>, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let groups = Promotions::new(store).entity_fact_groups(
        FACT_CATEGORY,
        Rung::FactToScenario.as_str(),
        MIN_SCENARIO_FACTS,
        MIN_SCENARIO_SOURCES,
        limit,
    )?;
    Ok(groups
        .into_iter()
        .map(|group| {
            let count = group.memory_ids.len();
            let name = group.entity_name;
            PromotionCandidate {
                rung: Rung::FactToScenario,
                snippet: format!("{} facts mentioning {}", count, name),
                reason: format!(
                    "{} un-promoted facts share the entity {:?} — enough for a scenario",
                    count, name
                ),
                grouped_by: Some(name),
                source_ids: group.memory_ids,
                refresh_of: None,
                new_evidence: Vec::new(),
            }
        })
        .collect())
}

/// Scenarios stable enough to say something durable, not yet in a persona.
fn persona_candidates(store: &Store<'_>, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let rows = Promotions::new(store).ready_scenarios(
        SCENARIO_CATEGORY,
        PERSONA_VITALITY_FLOOR,
        Rung::ScenarioToPersona.as_str(),
        limit,
    )?;
    Ok(rows
        .into_iter()
        .map(|row| PromotionCandidate {
            rung: Rung::ScenarioToPersona,
            source_ids: vec![row.id],
            snippet: snippet(&row.content),
            reason: format!(
                "scenario still at vitality {:.2}, above the {:.2} persona floor",
                row.vitality, PERSONA_VITALITY_FLOOR
            ),
            grouped_by: None,
            refresh_of: None,
            new_evidence: Vec::new(),
        })
        .collect())
}

/// What is ready to move up a rung.
///
/// Every rung's query excludes what has already been promoted, so running this
/// twice against unchanged data returns the same list and promoting from it
/// twice is impossible — idempotency comes from the candidate query, not from
/// the caller remembering.
pub fn promotion_candidates(
    store: &Store<'_>,
    rung: Rung,
    limit: usize,
) -> SqlResult<Vec<PromotionCandidate>> {
    let limit = limit.clamp(PROMOTION_LIMIT_MIN, PROMOTION_LIMIT_MAX);
    let fresh = |limit: usize| match rung {
        Rung::CaptureToFact => capture_candidates(store, limit),
        Rung::FactToScenario => scenario_candidates(store, limit),
        Rung::ScenarioToPersona => persona_candidates(store, limit),
    };
    if rung == Rung::CaptureToFact {
        return fresh(limit);
    }
    let mut out = refresh_candidates(store, rung, limit.div_ceil(2), Utc::now())?;
    out.extend(fresh(limit - out.len())?);
    Ok(out)
}

/// Stale artifacts at `rung`'s target, as `refresh` candidates, newest
/// first, at most `limit` (card #60).
///
/// They come before new promotions but take at most half the page, so a
/// store with many stale scenarios still shows what is new. A scenario's
/// candidate carries its facts plus the new evidence, ready to promote; a
/// persona statement's carries its scenarios, as new facts reach it only
/// through a refreshed scenario. [`count_candidates`] counts new promotions
/// only.
fn refresh_candidates(
    store: &Store<'_>,
    rung: Rung,
    limit: usize,
    now: chrono::DateTime<Utc>,
) -> SqlResult<Vec<PromotionCandidate>> {
    let category = match rung {
        Rung::FactToScenario => SCENARIO_CATEGORY,
        Rung::ScenarioToPersona => PERSONA_CATEGORY,
        Rung::CaptureToFact => return Ok(Vec::new()),
    };
    let promotions = Promotions::new(store);
    let mut out = Vec::new();
    let statements = promotions.statements_newest_first(category)?;
    for row in statements.into_iter().take(REFRESH_CHECK_MAX) {
        if out.len() >= limit {
            break;
        }
        // The listing has no sensitivity opt-in, so a sensitive artifact
        // is never offered, as in `persona`.
        if promotions.live_source_sensitivity(&row.id)? != Some(false) {
            continue;
        }
        // `persona` withholds an artifact with no surviving source (all
        // deleted, superseded, or reverted or abandoned decisions), so a
        // refresh of it would be withheld too.
        if surviving_sources(store, &row.id)? == 0 {
            continue;
        }
        // A pin is a person's line: never offered for a rewrite that would
        // supersede it (pack D102). It still reads as stale.
        if crate::kinds::is_pinned(&row.metadata) {
            continue;
        }
        let mut source_ids = usable_sources(&promotions, &row.id, rung)?;
        if source_ids.is_empty() {
            continue;
        }
        let staleness = staleness_of(store, &row.id, now)?;
        if !staleness.is_stale() {
            continue;
        }
        if rung == Rung::FactToScenario {
            source_ids.extend(staleness.new_evidence.iter().cloned());
        }
        out.push(PromotionCandidate {
            rung,
            source_ids,
            snippet: snippet(&row.content),
            reason: format!(
                "refresh: {} newer memories share an entity with what this was built from",
                staleness.new_evidence.len()
            ),
            grouped_by: None,
            refresh_of: Some(row.id),
            new_evidence: staleness.new_evidence,
        });
    }
    Ok(out)
}

/// The sources of `promoted_id` that [`promote`] would accept at `rung`:
/// live and unsuperseded, and not sensitive for a persona statement. A
/// withdrawn source is left out, so the candidate stays promotable.
fn usable_sources(
    promotions: &Promotions<'_>,
    promoted_id: &str,
    rung: Rung,
) -> SqlResult<Vec<String>> {
    let mut out = Vec::new();
    for id in promotions.sources_of(promoted_id)? {
        let usable = match promotions.live_source_sensitivity(&id)? {
            None => false,
            Some(sensitive) => !(sensitive && rung == Rung::ScenarioToPersona),
        };
        if usable {
            out.push(id);
        }
    }
    Ok(out)
}

/// Most promoted statements one candidate listing checks for staleness.
const REFRESH_CHECK_MAX: usize = PROMOTION_LIMIT_MAX;

/// Everything a promoted memory was built from, down to the fact rung: a
/// scenario's facts, or a persona statement's scenarios and their facts.
pub fn sources_down_to_facts(store: &Store<'_>, promoted_id: &str) -> SqlResult<Vec<String>> {
    let promotions = Promotions::new(store);
    let mut out: Vec<String> = Vec::new();
    let mut frontier = vec![promoted_id.to_string()];
    // Two hops cover the ladder: persona → scenario → fact.
    for _ in 0..2 {
        let mut next = Vec::new();
        for id in &frontier {
            for source in promotions.sources_of(id)? {
                if !out.contains(&source) {
                    out.push(source.clone());
                    next.push(source);
                }
            }
        }
        frontier = next;
    }
    Ok(out)
}

/// [`crate::staleness::stale_check`] for a promoted memory, against the
/// last revision of its content (D84): an artifact rewritten after the
/// evidence arrived is not stale for it. Unchecked when the memory is gone.
pub fn staleness_of(
    store: &Store<'_>,
    promoted_id: &str,
    now: chrono::DateTime<Utc>,
) -> SqlResult<crate::staleness::Staleness> {
    let rows = load_memories(store, &[promoted_id.to_string()])?;
    let Some(row) = rows.first() else {
        return Ok(crate::staleness::Staleness::unchecked());
    };
    let revised = content_revised_at(store, &row.id, &row.content, &row.created_at)?;
    let sources = sources_down_to_facts(store, promoted_id)?;
    crate::staleness::stale_check(store, &sources, &revised, now)
}

/// When memory `id`'s content last changed: the newest revision whose
/// snapshot differs from the content that replaced it, else `created_at`.
///
/// Not `updated_at`, which reminders, tag edits and sync bookkeeping stamp
/// without touching what the artifact says; an operational write must not
/// hide the evidence it predates.
fn content_revised_at(
    store: &Store<'_>,
    id: &str,
    content: &str,
    created_at: &str,
) -> SqlResult<String> {
    let revised = crate::db::history::Revisions::new(store).content_revised_at(id, content)?;
    Ok(revised.unwrap_or_else(|| created_at.to_string()))
}

/// [`persona`], each statement with its stale flag (card #60).
///
/// A separate call so the search bootstrap and the brief, which read the
/// persona on every request, do not pay for the check.
pub fn persona_with_staleness(store: &Store<'_>) -> SqlResult<Vec<PersonaStatement>> {
    let now = Utc::now();
    let mut statements = persona(store)?;
    for statement in &mut statements {
        let staleness = staleness_of(store, &statement.id, now)?;
        statement.stale = staleness.stale;
        statement.new_evidence = staleness.new_evidence;
    }
    Ok(statements)
}

/// The persona card `remind_me_persona` serves: at most
/// [`PERSONA_CARD_MAX`] statements of [`persona`], pinned first, each with
/// its stale flag, and how many stand in all.
pub fn persona_card(store: &Store<'_>) -> SqlResult<(Vec<PersonaStatement>, usize)> {
    let now = Utc::now();
    let mut statements = persona(store)?;
    let total = statements.len();
    statements.truncate(PERSONA_CARD_MAX);
    for statement in &mut statements {
        let staleness = staleness_of(store, &statement.id, now)?;
        statement.stale = staleness.stale;
        statement.new_evidence = staleness.new_evidence;
    }
    Ok((statements, total))
}

/// The true, uncapped count of what is ready at one rung.
///
/// Unlike [`backlog`], which probes every rung up to [`BACKLOG_PROBE`] so it
/// stays cheap when read alongside a candidate listing, this counts exactly —
/// no `LIMIT`, no probe ceiling — the same predicate [`promotion_candidates`]
/// filters by. It exists so a caller reading one rung's candidate page can
/// tell whether the page is the whole backlog or just the front of it (#283),
/// the way `recalibrate::candidate_count` already does for recalibration.
pub fn count_candidates(store: &Store<'_>, rung: Rung) -> SqlResult<usize> {
    let promotions = Promotions::new(store);
    match rung {
        Rung::CaptureToFact => promotions.count_undecomposed_dialogs(),
        Rung::FactToScenario => promotions.count_entity_fact_groups(
            FACT_CATEGORY,
            Rung::FactToScenario.as_str(),
            MIN_SCENARIO_FACTS,
            MIN_SCENARIO_SOURCES,
        ),
        Rung::ScenarioToPersona => promotions.count_ready_scenarios(
            SCENARIO_CATEGORY,
            PERSONA_VITALITY_FLOOR,
            Rung::ScenarioToPersona.as_str(),
        ),
    }
}

/// The already-promoted memory whose source set exactly matches `source_ids`
/// at `rung`, if its promoted memory is still live (not deleted, not
/// superseded).
///
/// `INSERT OR IGNORE` on `promotions` only dedupes rows *within* one call's
/// source list — nothing stopped two separate `promote()` calls with
/// identical `source_ids` from minting two independent promoted memories
/// from the same evidence (#274). This is the check that closes that gap:
/// same rung, same source set as an unordered set (order and any accidental
/// duplicates in the caller's list don't matter), and the promotion has to
/// still be live — a superseded or deleted promoted memory no longer counts,
/// so re-promoting the same sources after that is a fresh, legitimate call.
fn existing_live_promotion(
    store: &Store<'_>,
    rung: Rung,
    source_ids: &[String],
) -> SqlResult<Option<String>> {
    let mut wanted: Vec<String> = source_ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();

    // Any promoted_id sharing at least one source at this rung is a
    // candidate; each is then checked for an exact set match and liveness.
    // Cheap in practice: fan-out per source is small, and this only runs on
    // the write path, not the hot read path.
    let promotions = Promotions::new(store);
    for promoted_id in promotions.promoted_from(&wanted[0], rung.as_str())? {
        let mut existing = promotions.sources_at(&promoted_id, rung.as_str())?;
        existing.sort_unstable();
        existing.dedup();
        if existing != wanted {
            continue;
        }
        if promotions.is_live(&promoted_id)? {
            return Ok(Some(promoted_id));
        }
    }
    Ok(None)
}

/// Accept a distillation and record what it was built from.
pub fn promote(store: &Store<'_>, input: &PromoteInput) -> Result<PromotionResult, PromotionError> {
    promote_pausing(store, input, &|| {})
}

/// [`promote`], calling `after_checks` once every check has passed and
/// before anything is written: where a concurrent promotion would otherwise
/// slip in. For tests that put one there.
#[doc(hidden)]
pub fn promote_pausing(
    store: &Store<'_>,
    input: &PromoteInput,
    after_checks: &dyn Fn(),
) -> Result<PromotionResult, PromotionError> {
    if input.rung == Rung::CaptureToFact {
        return Err(PromotionError::UseDecompose);
    }
    if input.content.trim().is_empty() {
        return Err(PromotionError::EmptyContent);
    }
    if input.source_ids.is_empty() {
        return Err(PromotionError::NoSources);
    }

    let category = match input.rung {
        Rung::FactToScenario => SCENARIO_CATEGORY,
        Rung::ScenarioToPersona => PERSONA_CATEGORY,
        Rung::CaptureToFact => unreachable!("refused above"),
    };
    store.atomically(|store| promote_in(store, input, category, after_checks))
}

/// [`promote`]'s checks and writes, inside one transaction: the sources,
/// the refresh target and the duplicate-evidence check are read under the
/// same lock as the write, so two concurrent promotions cannot both pass
/// them and leave two live artifacts.
fn promote_in(
    store: &Store<'_>,
    input: &PromoteInput,
    category: &str,
    after_checks: &dyn Fn(),
) -> Result<PromotionResult, PromotionError> {
    // Validated before anything is written, so a promotion naming one bad
    // source does not leave a partially-linked artifact behind.
    for id in &input.source_ids {
        let sensitive = Promotions::new(store)
            .live_source_sensitivity(id)?
            .ok_or_else(|| PromotionError::UnusableSource(id.clone()))?;
        if sensitive && input.rung == Rung::ScenarioToPersona {
            return Err(PromotionError::SensitiveSource(id.clone()));
        }
    }

    if let Some(old) = &input.refreshes {
        check_refresh(store, old, category)?;
    }

    // Same evidence, promoted twice, is not a new promotion — see
    // `existing_live_promotion` for why "live" is the bar rather than "ever
    // promoted". Refreshing that very promotion is the exception: a persona
    // statement refreshed for new facts keeps the same scenarios.
    if let Some(promoted_id) = existing_live_promotion(store, input.rung, &input.source_ids)? {
        if input.refreshes.as_deref() != Some(promoted_id.as_str()) {
            return Err(PromotionError::DuplicateSources(promoted_id));
        }
    }

    let now_iso = Utc::now().to_rfc3339();
    let now = Utc::now();
    let promoted_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    // A distillation is a model's conclusion from its sources (#1553): a
    // pattern when seen across two or more distinct ones, else an inference
    // from one. A source named twice is still one premise.
    let mut premises: Vec<&String> = Vec::with_capacity(input.source_ids.len());
    for id in &input.source_ids {
        if !premises.contains(&id) {
            premises.push(id);
        }
    }
    let basis = if premises.len() >= 2 {
        crate::kinds::Basis::Pattern
    } else {
        crate::kinds::Basis::Inferred
    };
    let metadata = serde_json::json!({
        "rung": input.rung.as_str(),
        "promoted_from": input.source_ids,
        "basis": basis.as_str(),
        "premises": premises,
    });

    let decay_rate = get_decay_rate(category);
    let base_weight = get_type_prior(category) * get_source_prior(PROMOTION_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    let provenance = crate::context::default_provenance().auto();
    after_checks();
    Memories::new(store).insert(&NewMemory {
        category: category.to_string(),
        source: PROMOTION_SOURCE.to_string(),
        metadata,
        decay_rate,
        vitality,
        base_weight,
        accessed_at: Some(now_iso.clone()),
        ..provenance.stamp(NewMemory::new(
            promoted_id.clone(),
            input.content.clone(),
            &now_iso,
        ))
    })?;

    let promotions = Promotions::new(store);
    for source_id in &input.source_ids {
        promotions.record(&promoted_id, source_id, input.rung.as_str(), &now_iso)?;
    }
    if let Some(old) = &input.refreshes {
        Memories::new(store).set_superseded_by(old, &promoted_id, Some(&now_iso))?;
    }

    Ok(PromotionResult {
        promoted_id,
        rung: input.rung,
        category: category.to_string(),
        source_ids: input.source_ids.clone(),
    })
}

/// Refuse a `refreshes` that names no live promoted memory in `category`.
fn check_refresh(store: &Store<'_>, old: &str, category: &str) -> Result<(), PromotionError> {
    let unusable = || PromotionError::UnusableRefresh(old.to_string());
    let rows = load_memories(store, &[old.to_string()])?;
    let row = rows.first().ok_or_else(unusable)?;
    let live = row.superseded_by.is_none() && row.deleted_at.is_none();
    if !live || row.category != category || row.source != PROMOTION_SOURCE {
        return Err(unusable());
    }
    // A pinned line is a person's: no refresh supersedes it (pack D102).
    if crate::kinds::is_pinned(&row.metadata) {
        return Err(PromotionError::PinnedRefresh(old.to_string()));
    }
    Ok(())
}

/// What a memory was built from, and what was built on it.
///
/// Both directions from one indexed table, so "explain this persona statement"
/// and "what does this fact still support" cost the same.
pub fn provenance(store: &Store<'_>, memory_id: &str) -> SqlResult<Provenance> {
    let promotions = Promotions::new(store);
    let mut sources = promotions.sources_of(memory_id)?;
    let mut derived = promotions.derived_from(memory_id)?;

    // The capture a fact was decomposed from is its source; a capture's
    // decomposed facts are what was derived from it.
    let (capture_sources, capture_derived) = Memories::new(store).capture_links(memory_id)?;
    for (list, extra) in [
        (&mut sources, capture_sources),
        (&mut derived, capture_derived),
    ] {
        for id in extra {
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }

    Ok(Provenance {
        memory_id: memory_id.to_string(),
        sources,
        derived,
    })
}

/// How many of a promoted artifact's sources are still standing.
fn surviving_sources(store: &Store<'_>, promoted_id: &str) -> SqlResult<usize> {
    Promotions::new(store).surviving_sources(promoted_id)
}

/// The current persona: durable statements whose grounds still hold.
///
/// A statement every one of whose sources has been superseded or deleted is
/// **omitted rather than deleted**. Demotion is a read-time judgement, so a
/// fact restored (or a supersession undone) brings its persona statement back
/// without anything having to notice; deleting the row would make that
/// one-way. The row also stays as the record of what was once believed and
/// why, which is the thing provenance exists to preserve.
///
/// Sensitive statements are excluded with no override, matching
/// [`crate::digest`]: a persona is an ambient surface, assembled to be
/// injected rather than asked for, so there is no per-call intent to opt back
/// in against.
pub fn persona(store: &Store<'_>) -> SqlResult<Vec<PersonaStatement>> {
    let rows = Promotions::new(store).statements_by_vitality(PERSONA_CATEGORY)?;

    let mut out = Vec::new();
    for row in rows {
        let surviving = surviving_sources(store, &row.id)?;
        if !held(store, &row, surviving)? {
            continue;
        }
        out.push(statement(row, surviving));
    }
    // Pinned lines first (#1555); the sort is stable, so each group keeps
    // the vitality order it was read in.
    out.sort_by_key(|s| !s.pinned);
    Ok(out)
}

/// Whether a persona row stands: a promoted line while any source does, a
/// pinned line regardless of its sources (pack D102). A pinned line with no
/// sources at all must have been written by a person; one that was not (it
/// can only predate the check at write time) is withheld.
fn held(store: &Store<'_>, row: &StatementRow, surviving: usize) -> SqlResult<bool> {
    if surviving > 0 {
        return Ok(true);
    }
    if !crate::kinds::is_pinned(&row.metadata) {
        return Ok(false);
    }
    if row.written_by == "human" {
        return Ok(true);
    }
    Ok(!Promotions::new(store).sources_of(&row.id)?.is_empty())
}

fn statement(row: StatementRow, surviving: usize) -> PersonaStatement {
    PersonaStatement {
        pinned: crate::kinds::is_pinned(&row.metadata),
        kind: crate::kinds::PersonaKind::of(&row.metadata),
        id: row.id,
        content: row.content,
        vitality: row.vitality,
        created_at: row.created_at,
        surviving_sources: surviving,
        stale: None,
        new_evidence: Vec::new(),
    }
}

/// Most statements `remind_me_persona` lists, pinned first (pack D101).
pub const PERSONA_CARD_MAX: usize = 40;

/// Persona statements currently withheld, and why — the other half of
/// [`persona`].
///
/// Without this, a statement that quietly stopped appearing is indistinguishable
/// from one that was never written. A caller asking "why did the assistant stop
/// believing that" has somewhere to look.
pub fn demoted(store: &Store<'_>) -> SqlResult<Vec<PersonaStatement>> {
    let rows = Promotions::new(store).statements_newest_first(PERSONA_CATEGORY)?;

    let mut out = Vec::new();
    for row in rows {
        let surviving = surviving_sources(store, &row.id)?;
        if !held(store, &row, surviving)? {
            out.push(statement(row, surviving));
        }
    }
    Ok(out)
}

/// Fraction of a search's token budget the L2/L3 bootstrap may spend.
pub const BOOTSTRAP_RESERVE_ENV: &str = "REMIND_ME_BOOTSTRAP_RESERVE";

/// Default share of the budget reserved for the bootstrap.
pub const BOOTSTRAP_RESERVE_DEFAULT: f64 = 0.25;

/// Hard ceiling on the reserve, applied after the environment is read.
///
/// This is the floor under the hits, expressed from the other side. A caller
/// who sets the reserve to `0.9` — or to `4.0` by fat-fingering a percentage —
/// gets `0.5`, so the ranked results always keep at least half the budget. A
/// bootstrap that can crowd out the answer to the actual query is worse than
/// no bootstrap: the caller asked a question, and the persona is context for
/// it rather than a substitute for it.
pub const BOOTSTRAP_RESERVE_MAX: f64 = 0.5;

/// Resolved reserve fraction, read fresh from the environment.
///
/// Read at call time rather than cached, matching `REMIND_ME_RRF_*`
/// ([`crate::retrieval`]) — a test that sets the variable must not depend on
/// having done so before some other test read it first.
///
/// Anything unparseable, negative or NaN falls back to the default rather than
/// erroring: this sits on the search hot path, and failing a search because a
/// tuning knob is malformed trades a small misconfiguration for a total
/// outage.
pub fn bootstrap_reserve_fraction() -> f64 {
    let raw = match std::env::var(BOOTSTRAP_RESERVE_ENV) {
        Ok(v) => v,
        Err(_) => return BOOTSTRAP_RESERVE_DEFAULT,
    };
    match raw.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => v.min(BOOTSTRAP_RESERVE_MAX),
        _ => BOOTSTRAP_RESERVE_DEFAULT,
    }
}

/// The L2/L3 context bootstrap for one search, and what it cost.
///
/// `omitted` is the same kind of number as [`crate::retrieval::TrimOutcome`]'s
/// `trimmed`, and exists for the same reason: a bootstrap cut down to fit is
/// otherwise indistinguishable from a persona that is genuinely that short.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Bootstrap {
    /// Statements that fit, most vital first.
    pub statements: Vec<PersonaStatement>,
    /// Estimated tokens across `statements`.
    pub tokens_used: usize,
    /// Surviving statements that did not fit the reserve.
    pub omitted: usize,
}

impl Bootstrap {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }
}

/// Assemble the persona bootstrap for a search with `token_budget` to spend.
///
/// # Why this goes through [`persona`]
///
/// It would be marginally cheaper to select the persona rows directly here.
/// Doing so would also duplicate two rules that are easy to get wrong and
/// invisible when wrong: demotion (a statement whose sources have all been
/// superseded is withheld) and the unconditional sensitive exclusion. A second
/// query would eventually drift from the first, and the failure mode is a
/// withdrawn — or sensitive — statement being injected into *every* search
/// while `remind_me_persona` correctly reports it as gone. One code path means
/// that cannot happen, and #255's acceptance asks for exactly that guarantee.
///
/// # Budget
///
/// `token_budget` is the search's whole budget; the reserve is taken from it
/// here so callers cannot forget to apply the ceiling. A budget of `0` means
/// unlimited, matching [`crate::retrieval::trim_by_token_budget`], and the
/// bootstrap is then uncapped too — the caller has said they want everything.
///
/// Statements are taken in `persona`'s order (most vital first), so what
/// survives a tight reserve is the most durable part of the persona rather
/// than whichever rows happened to sort first.
pub fn bootstrap(store: &Store<'_>, token_budget: usize) -> SqlResult<Bootstrap> {
    let statements = persona(store)?;
    let total = statements.len();

    if token_budget == 0 {
        let tokens_used = statements
            .iter()
            .map(|s| crate::retrieval::estimated_content_tokens(&s.content))
            .sum();
        return Ok(Bootstrap {
            statements,
            tokens_used,
            omitted: 0,
        });
    }

    let reserve = (token_budget as f64 * bootstrap_reserve_fraction()).floor() as usize;

    let mut tokens_used = 0usize;
    let mut kept = Vec::new();
    for statement in statements {
        let cost = crate::retrieval::estimated_content_tokens(&statement.content);
        // No `!kept.is_empty()` escape hatch, unlike `trim_by_token_budget`:
        // there, returning zero results would be a useless answer to a
        // question, so the first result is admitted over budget. Here an empty
        // bootstrap is a perfectly good outcome — the caller still gets their
        // search — and admitting one oversized statement anyway is how a
        // reserve stops being a bound.
        if tokens_used + cost > reserve {
            break;
        }
        tokens_used += cost;
        kept.push(statement);
    }

    Ok(Bootstrap {
        omitted: total - kept.len(),
        statements: kept,
        tokens_used,
    })
}

/// Seconds between backlog checks. Unset (or zero) means the nudge never runs.
pub const NUDGE_INTERVAL_ENV: &str = "REMIND_ME_PROMOTION_INTERVAL";

/// How many candidates the backlog count looks at per rung.
///
/// A count, not a listing, so this only bounds the work — a backlog of 5000
/// and one of 500 both read as "plenty", and the number exists to say whether
/// there is anything to do at all.
const BACKLOG_PROBE: usize = 100;

/// How much work is waiting at each rung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Backlog {
    pub capture_to_fact: usize,
    pub fact_to_scenario: usize,
    pub scenario_to_persona: usize,
}

impl Backlog {
    pub fn total(&self) -> usize {
        self.capture_to_fact + self.fact_to_scenario + self.scenario_to_persona
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// One line naming what is waiting, for a notification body.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (label, n) in [
            ("capture→fact", self.capture_to_fact),
            ("fact→scenario", self.fact_to_scenario),
            ("scenario→persona", self.scenario_to_persona),
        ] {
            if n > 0 {
                parts.push(format!("{} {}", n, label));
            }
        }
        if parts.is_empty() {
            "nothing waiting".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Count what is ready at every rung.
pub fn backlog(store: &Store<'_>) -> SqlResult<Backlog> {
    Ok(Backlog {
        capture_to_fact: promotion_candidates(store, Rung::CaptureToFact, BACKLOG_PROBE)?.len(),
        fact_to_scenario: promotion_candidates(store, Rung::FactToScenario, BACKLOG_PROBE)?.len(),
        scenario_to_persona: promotion_candidates(store, Rung::ScenarioToPersona, BACKLOG_PROBE)?
            .len(),
    })
}

/// Whether a nudge interval is configured at all.
pub fn nudge_interval() -> Option<std::time::Duration> {
    std::env::var(NUDGE_INTERVAL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
}

/// Tracks the last backlog total a notification was sent for.
///
/// Process-local rather than a table, and the choice is deliberate. A
/// notification is a *prompt to do work*, not a record that work happened —
/// the record is the `promotions` table. Persisting "already nudged" would
/// mean a backlog that is never worked eventually stops being mentioned,
/// which is the failure mode a nudge exists to prevent.
static LAST_NUDGED_TOTAL: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

/// One backlog check. Notifies only when the total has *changed*.
///
/// Re-announcing an unchanged backlog on every pass would make the channel
/// useless within a day — an hourly nudge saying the same thing is noise the
/// reader learns to filter, taking the real change with it. A backlog that
/// grows nudges again; one that sits still does not; one that is worked down
/// to zero says so once and then goes quiet.
///
/// Returns the backlog it observed, and whether it notified.
pub fn nudge_once(store: &Store<'_>) -> SqlResult<(Backlog, bool)> {
    let backlog = backlog(store)?;
    let total = backlog.total();

    let mut last = LAST_NUDGED_TOTAL.lock().unwrap_or_else(|e| e.into_inner());
    if *last == Some(total) {
        return Ok((backlog, false));
    }
    *last = Some(total);
    drop(last);

    if total == 0 {
        // Worth one line when the backlog clears, and nothing after.
        return Ok((backlog, false));
    }

    crate::notifications::notify(
        "Memory refinement backlog",
        &format!(
            "{} ready to promote: {}. Use remind_me_promotion_candidates to see them.",
            total,
            backlog.summary()
        ),
    );
    Ok((backlog, true))
}

/// The nudge loop this process is running, if any.
///
/// Mirrors [`crate::watcher`]'s own `LIVE`: a `Mutex<Option<..>>`, not a
/// `OnceLock`, so a stopped loop can clear itself rather than leave a
/// `Liveness` behind that would misreport a restarted loop's identity.
static LIVE: std::sync::Mutex<Option<crate::scheduler::Liveness>> = std::sync::Mutex::new(None);

/// Whether the backlog-nudge loop's thread is actually running right now.
///
/// Distinct from [`nudge_interval`] alone: that only says an interval is
/// *configured*, which [`start_nudge_for`] already requires before it ever
/// registers a loop here, so a crashed thread and an unconfigured one were
/// previously indistinguishable — both left a caller believing backlog
/// growth would eventually be announced when nothing was actually watching
/// for it (#270).
pub fn nudge_running() -> bool {
    LIVE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(crate::scheduler::Liveness::is_alive)
}

/// A running nudge loop. Dropping it does **not** stop the loop — call
/// [`NudgeHandle::stop`], which joins, so an in-flight pass cannot still be
/// reading while the caller tears the database down underneath it.
pub struct NudgeHandle {
    stop: std::sync::Arc<crate::scheduler::Stop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NudgeHandle {
    pub fn stop(mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Cleared after the join, matching `WatcherHandle::stop`'s own
        // reasoning: until the thread has actually finished, it is still
        // running and `nudge_running` should say so.
        *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Start the backlog nudge for the database `store` is attached to.
///
/// `None` when no interval is configured — unlike the reminder scheduler,
/// which always runs, this is opt-in, matching the folder watcher (#55) and
/// webhook (#56) convention. Also `None` for an in-memory database, which
/// lives only as long as its `Database`.
pub fn start_nudge_for(store: &Store<'_>) -> Option<NudgeHandle> {
    let interval = nudge_interval()?;
    Some(start_nudge(store.secondary_source()?, interval))
}

pub fn start_nudge(
    source: crate::db::SecondarySource,
    interval: std::time::Duration,
) -> NudgeHandle {
    let stop = std::sync::Arc::new(crate::scheduler::Stop::new());
    let loop_stop = std::sync::Arc::clone(&stop);
    let (liveness, liveness_guard) = crate::scheduler::Liveness::new();
    *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(liveness);

    let thread = std::thread::Builder::new()
        .name("promotion-nudge".to_string())
        .spawn(move || {
            let _liveness_guard = liveness_guard;
            let store = source.store();
            while !loop_stop.is_stopped() {
                // A failed pass is reported and the loop continues: a
                // transient database error must not silently end the nudge
                // for the rest of the process's life.
                match nudge_once(&store) {
                    Ok((backlog, true)) => {
                        eprintln!("promotion nudge: {}", backlog.summary())
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("promotion nudge: pass failed: {}", e),
                }
                loop_stop.wait(interval);
            }
        })
        .expect("spawning the promotion nudge thread");

    NudgeHandle {
        stop,
        thread: Some(thread),
    }
}

/// Load the memories a set of ids names, in the order given, skipping any that
/// no longer resolve. Used by the tool layer to show a candidate's sources.
pub fn load_memories(store: &Store<'_>, ids: &[String]) -> SqlResult<Vec<Memory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Memories::new(store).get_many(ids)
}
