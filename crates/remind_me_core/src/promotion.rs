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
//! # Not in the generated schema
//!
//! `promotions` is target-only, created by [`ensure_schema`] the way
//! `vectors::ensure_schema` creates `vec_embeddings` and
//! `archive::ensure_schema` creates its own. `db/schema_tables.sql` is
//! generated verbatim from `remind_me` and is not this crate's to extend.

use crate::models::{
    Memory, PersonaStatement, PromoteInput, PromotionCandidate, PromotionResult, Provenance, Rung,
    FACT_CATEGORY, PERSONA_CATEGORY, PROMOTION_LIMIT_MAX, PROMOTION_LIMIT_MIN, SCENARIO_CATEGORY,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, params_from_iter, Connection, Result as SqlResult};

/// `source` promoted artifacts are stored under.
pub const PROMOTION_SOURCE: &str = "promotion";

/// Fewest facts sharing an entity before a scenario is worth proposing.
///
/// Two facts about the same entity is a coincidence; the threshold is what
/// stops the candidate list being one entry per entity in the store from the
/// moment anything is decomposed.
pub const MIN_SCENARIO_FACTS: usize = 3;

/// Vitality a scenario must still hold to be persona material.
///
/// The persona rung is meant to be *stable*, so the gate is the decay model
/// already in use rather than a new stability counter: a scenario that has not
/// been touched since it was written falls below this on its own.
pub const PERSONA_VITALITY_FLOOR: f64 = 0.5;

/// Longest snippet carried in a candidate listing.
const SNIPPET_CHARS: usize = 200;

/// Create this crate's own promotion-provenance table, if absent.
///
/// Called from [`crate::db::schema::initialize_schema`] after the generated
/// schema is applied. Created unconditionally: an empty table costs nothing,
/// and a lazily-created one would make every read path tolerate its absence.
///
/// No foreign keys. A promoted memory can be deleted through the ordinary
/// delete path, and a cascade would erase the provenance that says what it
/// *was* derived from — which is exactly the record needed to explain why a
/// persona statement vanished.
pub fn ensure_schema(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS promotions (
            promoted_id TEXT NOT NULL,
            source_id   TEXT NOT NULL,
            rung        TEXT NOT NULL,
            promoted_at TEXT NOT NULL,
            PRIMARY KEY (promoted_id, source_id)
         );
         CREATE INDEX IF NOT EXISTS idx_promotions_source
            ON promotions(source_id);",
    )
}

/// Why a promotion could not be made.
#[derive(Debug)]
pub enum PromotionError {
    Db(rusqlite::Error),
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
        }
    }
}

impl std::error::Error for PromotionError {}

impl From<rusqlite::Error> for PromotionError {
    fn from(e: rusqlite::Error) -> Self {
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
fn capture_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content
           FROM memories m
          WHERE m.capture_id IS NOT NULL
            AND m.source_capture_id IS NULL
            AND m.deleted_at IS NULL
            AND m.category = 'dialog'
            AND NOT EXISTS (
                SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
            )
          ORDER BY m.created_at DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        let id: String = r.get(0)?;
        let content: String = r.get(1)?;
        Ok(PromotionCandidate {
            rung: Rung::CaptureToFact,
            source_ids: vec![id],
            snippet: snippet(&content),
            reason: "captured but never decomposed into facts".to_string(),
            grouped_by: None,
        })
    })?;
    rows.collect()
}

/// Facts clustered by a shared entity, where no scenario has been built from
/// them yet.
///
/// Grouping by entity rather than by embedding similarity on purpose:
/// `consolidation.rs` already clusters on cosine distance to find things that
/// are *the same*, and this rung wants things that are *related but distinct*.
/// The entity graph is the existing structure that expresses that.
fn scenario_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.name, group_concat(m.id), count(*) AS n
           FROM memories m
           JOIN memory_entities me ON me.memory_id = m.id
           JOIN entities e ON e.id = me.entity_id
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )
          GROUP BY e.id
         HAVING n >= ?
          ORDER BY n DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![
            FACT_CATEGORY,
            Rung::FactToScenario.as_str(),
            MIN_SCENARIO_FACTS as i64,
            limit as i64
        ],
        |r| {
            let name: String = r.get(1)?;
            let ids: String = r.get(2)?;
            let count: i64 = r.get(3)?;
            let source_ids: Vec<String> = ids.split(',').map(str::to_string).collect();
            Ok(PromotionCandidate {
                rung: Rung::FactToScenario,
                snippet: format!("{} facts mentioning {}", count, name),
                reason: format!(
                    "{} un-promoted facts share the entity {:?} — enough for a scenario",
                    count, name
                ),
                grouped_by: Some(name),
                source_ids,
            })
        },
    )?;
    rows.collect()
}

/// Scenarios stable enough to say something durable, not yet in a persona.
fn persona_candidates(conn: &Connection, limit: usize) -> SqlResult<Vec<PromotionCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content, m.vitality
           FROM memories m
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND m.sensitive = 0
            AND m.vitality >= ?
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )
          ORDER BY m.vitality DESC
          LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![
            SCENARIO_CATEGORY,
            PERSONA_VITALITY_FLOOR,
            Rung::ScenarioToPersona.as_str(),
            limit as i64
        ],
        |r| {
            let id: String = r.get(0)?;
            let content: String = r.get(1)?;
            let vitality: f64 = r.get(2)?;
            Ok(PromotionCandidate {
                rung: Rung::ScenarioToPersona,
                source_ids: vec![id],
                snippet: snippet(&content),
                reason: format!(
                    "scenario still at vitality {:.2}, above the {:.2} persona floor",
                    vitality, PERSONA_VITALITY_FLOOR
                ),
                grouped_by: None,
            })
        },
    )?;
    rows.collect()
}

/// What is ready to move up a rung.
///
/// Every rung's query excludes what has already been promoted, so running this
/// twice against unchanged data returns the same list and promoting from it
/// twice is impossible — idempotency comes from the candidate query, not from
/// the caller remembering.
pub fn promotion_candidates(
    conn: &Connection,
    rung: Rung,
    limit: usize,
) -> SqlResult<Vec<PromotionCandidate>> {
    let limit = limit.clamp(PROMOTION_LIMIT_MIN, PROMOTION_LIMIT_MAX);
    match rung {
        Rung::CaptureToFact => capture_candidates(conn, limit),
        Rung::FactToScenario => scenario_candidates(conn, limit),
        Rung::ScenarioToPersona => persona_candidates(conn, limit),
    }
}

/// The true, uncapped count of what is ready at one rung.
///
/// Unlike [`backlog`], which probes every rung up to [`BACKLOG_PROBE`] so it
/// stays cheap when read alongside a candidate listing, this counts exactly —
/// no `LIMIT`, no probe ceiling — the same predicate [`promotion_candidates`]
/// filters by. It exists so a caller reading one rung's candidate page can
/// tell whether the page is the whole backlog or just the front of it (#283),
/// the way `recalibrate::candidate_count` already does for recalibration.
pub fn count_candidates(conn: &Connection, rung: Rung) -> SqlResult<usize> {
    let count: i64 = match rung {
        Rung::CaptureToFact => conn.query_row(
            "SELECT COUNT(*)
               FROM memories m
              WHERE m.capture_id IS NOT NULL
                AND m.source_capture_id IS NULL
                AND m.deleted_at IS NULL
                AND m.category = 'dialog'
                AND NOT EXISTS (
                    SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
                )",
            [],
            |r| r.get(0),
        )?,
        Rung::FactToScenario => conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT e.id
                   FROM memories m
                   JOIN memory_entities me ON me.memory_id = m.id
                   JOIN entities e ON e.id = me.entity_id
                  WHERE m.category = ?
                    AND m.deleted_at IS NULL
                    AND m.superseded_by IS NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
                    )
                  GROUP BY e.id
                 HAVING count(*) >= ?
             )",
            params![
                FACT_CATEGORY,
                Rung::FactToScenario.as_str(),
                MIN_SCENARIO_FACTS as i64
            ],
            |r| r.get(0),
        )?,
        Rung::ScenarioToPersona => conn.query_row(
            "SELECT COUNT(*)
               FROM memories m
              WHERE m.category = ?
                AND m.deleted_at IS NULL
                AND m.superseded_by IS NULL
                AND m.sensitive = 0
                AND m.vitality >= ?
                AND NOT EXISTS (
                    SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
                )",
            params![
                SCENARIO_CATEGORY,
                PERSONA_VITALITY_FLOOR,
                Rung::ScenarioToPersona.as_str()
            ],
            |r| r.get(0),
        )?,
    };
    Ok(count as usize)
}

/// One source memory, as far as promotion cares.
struct SourceRow {
    sensitive: bool,
}

fn load_source(conn: &Connection, id: &str) -> SqlResult<Option<SourceRow>> {
    conn.query_row(
        "SELECT sensitive FROM memories
          WHERE id = ? AND deleted_at IS NULL AND superseded_by IS NULL",
        params![id],
        |r| {
            Ok(SourceRow {
                sensitive: r.get(0)?,
            })
        },
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    })
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
    conn: &Connection,
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
    let mut candidates_stmt = conn
        .prepare("SELECT DISTINCT promoted_id FROM promotions WHERE source_id = ? AND rung = ?")?;
    let mut sources_stmt =
        conn.prepare("SELECT source_id FROM promotions WHERE promoted_id = ? AND rung = ?")?;
    let mut live_stmt = conn.prepare(
        "SELECT count(*) FROM memories
          WHERE id = ? AND deleted_at IS NULL AND superseded_by IS NULL",
    )?;

    let candidate_ids: Vec<String> = candidates_stmt
        .query_map(params![wanted[0], rung.as_str()], |r| r.get(0))?
        .collect::<SqlResult<_>>()?;

    for promoted_id in candidate_ids {
        let mut existing: Vec<String> = sources_stmt
            .query_map(params![promoted_id, rung.as_str()], |r| r.get(0))?
            .collect::<SqlResult<_>>()?;
        existing.sort_unstable();
        existing.dedup();
        if existing != wanted {
            continue;
        }
        let live: i64 = live_stmt.query_row(params![promoted_id], |r| r.get(0))?;
        if live > 0 {
            return Ok(Some(promoted_id));
        }
    }
    Ok(None)
}

/// Accept a distillation and record what it was built from.
pub fn promote(conn: &Connection, input: &PromoteInput) -> Result<PromotionResult, PromotionError> {
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

    // Validated before anything is written, so a promotion naming one bad
    // source does not leave a partially-linked artifact behind.
    for id in &input.source_ids {
        let source =
            load_source(conn, id)?.ok_or_else(|| PromotionError::UnusableSource(id.clone()))?;
        if source.sensitive && input.rung == Rung::ScenarioToPersona {
            return Err(PromotionError::SensitiveSource(id.clone()));
        }
    }

    // Same evidence, promoted twice, is not a new promotion — see
    // `existing_live_promotion` for why "live" is the bar rather than "ever
    // promoted".
    if let Some(promoted_id) = existing_live_promotion(conn, input.rung, &input.source_ids)? {
        return Err(PromotionError::DuplicateSources(promoted_id));
    }

    let now_iso = Utc::now().to_rfc3339();
    let now = Utc::now();
    let promoted_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    let metadata = serde_json::json!({
        "rung": input.rung.as_str(),
        "promoted_from": input.source_ids,
    });

    let decay_rate = get_decay_rate(category);
    let base_weight = get_type_prior(category) * get_source_prior(PROMOTION_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    let (node_id, client) = crate::sync::memory_provenance();
    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata,
            created_at, updated_at, decay_rate, vitality, base_weight,
            access_count, accessed_at, node_id, client
         ) VALUES (?, ?, ?, '[]', ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
        params![
            promoted_id,
            input.content,
            category,
            PROMOTION_SOURCE,
            metadata.to_string(),
            now_iso,
            now_iso,
            decay_rate,
            vitality,
            base_weight,
            now_iso,
            node_id,
            client,
        ],
    )?;

    for source_id in &input.source_ids {
        conn.execute(
            "INSERT OR IGNORE INTO promotions (promoted_id, source_id, rung, promoted_at)
             VALUES (?, ?, ?, ?)",
            params![promoted_id, source_id, input.rung.as_str(), now_iso],
        )?;
    }

    Ok(PromotionResult {
        promoted_id,
        rung: input.rung,
        category: category.to_string(),
        source_ids: input.source_ids.clone(),
    })
}

/// What a memory was built from, and what was built on it.
///
/// Both directions from one indexed table, so "explain this persona statement"
/// and "what does this fact still support" cost the same.
pub fn provenance(conn: &Connection, memory_id: &str) -> SqlResult<Provenance> {
    let mut up = conn.prepare("SELECT source_id FROM promotions WHERE promoted_id = ?")?;
    let sources: Vec<String> = up
        .query_map(params![memory_id], |r| r.get(0))?
        .collect::<SqlResult<_>>()?;

    let mut down = conn.prepare("SELECT promoted_id FROM promotions WHERE source_id = ?")?;
    let derived: Vec<String> = down
        .query_map(params![memory_id], |r| r.get(0))?
        .collect::<SqlResult<_>>()?;

    Ok(Provenance {
        memory_id: memory_id.to_string(),
        sources,
        derived,
    })
}

/// How many of a promoted artifact's sources are still standing.
fn surviving_sources(conn: &Connection, promoted_id: &str) -> SqlResult<usize> {
    let count: i64 = conn.query_row(
        "SELECT count(*)
           FROM promotions p
           JOIN memories m ON m.id = p.source_id
          WHERE p.promoted_id = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL",
        params![promoted_id],
        |r| r.get(0),
    )?;
    Ok(count as usize)
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
pub fn persona(conn: &Connection) -> SqlResult<Vec<PersonaStatement>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, vitality, created_at
           FROM memories
          WHERE category = ?
            AND deleted_at IS NULL
            AND superseded_by IS NULL
            AND sensitive = 0
          ORDER BY vitality DESC",
    )?;
    let rows: Vec<(String, String, f64, String)> = stmt
        .query_map(params![PERSONA_CATEGORY], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<SqlResult<_>>()?;

    let mut out = Vec::new();
    for (id, content, vitality, created_at) in rows {
        let surviving = surviving_sources(conn, &id)?;
        if surviving == 0 {
            continue;
        }
        out.push(PersonaStatement {
            id,
            content,
            vitality,
            created_at,
            surviving_sources: surviving,
        });
    }
    Ok(out)
}

/// Persona statements currently withheld, and why — the other half of
/// [`persona`].
///
/// Without this, a statement that quietly stopped appearing is indistinguishable
/// from one that was never written. A caller asking "why did the assistant stop
/// believing that" has somewhere to look.
pub fn demoted(conn: &Connection) -> SqlResult<Vec<PersonaStatement>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, vitality, created_at
           FROM memories
          WHERE category = ?
            AND deleted_at IS NULL
            AND superseded_by IS NULL
          ORDER BY created_at DESC",
    )?;
    let rows: Vec<(String, String, f64, String)> = stmt
        .query_map(params![PERSONA_CATEGORY], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<SqlResult<_>>()?;

    let mut out = Vec::new();
    for (id, content, vitality, created_at) in rows {
        if surviving_sources(conn, &id)? == 0 {
            out.push(PersonaStatement {
                id,
                content,
                vitality,
                created_at,
                surviving_sources: 0,
            });
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
pub fn bootstrap(conn: &Connection, token_budget: usize) -> SqlResult<Bootstrap> {
    let statements = persona(conn)?;
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
pub fn backlog(conn: &Connection) -> SqlResult<Backlog> {
    Ok(Backlog {
        capture_to_fact: promotion_candidates(conn, Rung::CaptureToFact, BACKLOG_PROBE)?.len(),
        fact_to_scenario: promotion_candidates(conn, Rung::FactToScenario, BACKLOG_PROBE)?.len(),
        scenario_to_persona: promotion_candidates(conn, Rung::ScenarioToPersona, BACKLOG_PROBE)?
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
pub fn nudge_once(conn: &Connection) -> SqlResult<(Backlog, bool)> {
    let backlog = backlog(conn)?;
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

/// Start the backlog nudge for the database `conn` is attached to.
///
/// `None` when no interval is configured — unlike the reminder scheduler,
/// which always runs, this is opt-in, matching the folder watcher (#55) and
/// webhook (#56) convention. Also `None` for an in-memory database: the
/// thread opens its own connection by path, and `:memory:` would give it a
/// different, empty one, so it would report an empty backlog forever.
pub fn start_nudge_for(conn: &Connection) -> Option<NudgeHandle> {
    let interval = nudge_interval()?;
    let path: String = conn
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .ok()?;
    if path.is_empty() {
        return None;
    }
    Some(start_nudge(std::path::PathBuf::from(path), interval))
}

pub fn start_nudge(db_path: std::path::PathBuf, interval: std::time::Duration) -> NudgeHandle {
    let stop = std::sync::Arc::new(crate::scheduler::Stop::new());
    let loop_stop = std::sync::Arc::clone(&stop);
    let (liveness, liveness_guard) = crate::scheduler::Liveness::new();
    *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(liveness);

    let thread = std::thread::Builder::new()
        .name("promotion-nudge".to_string())
        .spawn(move || {
            let _liveness_guard = liveness_guard;
            let conn = match Connection::open(&db_path) {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("promotion nudge: cannot open {:?}: {}", db_path, e);
                    return;
                }
            };
            while !loop_stop.is_stopped() {
                // A failed pass is reported and the loop continues: a
                // transient database error must not silently end the nudge
                // for the rest of the process's life.
                match nudge_once(&conn) {
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
pub fn load_memories(conn: &Connection, ids: &[String]) -> SqlResult<Vec<Memory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; ids.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE id IN ({})",
        crate::db::queries::MEMORY_COLUMNS,
        marks
    ))?;
    let rows = stmt.query_map(
        params_from_iter(ids.iter()),
        crate::db::queries::parse_memory_row,
    )?;
    rows.collect()
}
