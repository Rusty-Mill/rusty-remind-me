//! The curation queues on the engine core (ADR-0023, core PR 3c, built
//! dark): captures awaiting decomposition, raw imports awaiting
//! normalization, contradiction candidates and the maintenance counts.
//! This is the engine's [`CurationStore`] adapter (ADR-0035).

use super::core::CoreTables;
use super::memories::{self, MemoryRow};
use super::{core_ref, EngineTables};
use crate::db::curation::{
    Backlog, CaptureActivity, CaptureRow, CurationStore, ImportRow, NormalizationSource,
    RefineObservation,
};
use crate::db::{Result, StoreError};
use crate::models::{ContradictionSide, Memory};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub(crate) fn live(row: &MemoryRow) -> bool {
    row.deleted_at.is_none() && row.superseded_by.is_none()
}

pub(crate) fn newest_first(rows: &mut [MemoryRow]) {
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
}

pub(crate) fn tags(row: &MemoryRow) -> Vec<String> {
    serde_json::from_str(&row.tags).unwrap_or_default()
}

// --- captures ---------------------------------------------------------------

/// The revision of the capture a fact was drawn from: its
/// `metadata.capture_revision` when that is an RFC 3339 time no later than
/// the fact itself, else its own `created_at`. A marker that is not a time,
/// or lies in the fact's future, was never validated and is ignored.
fn drawn_from(row: &MemoryRow) -> String {
    drawn_at(&row.metadata, &row.created_at)
}

/// [`drawn_from`] over the two columns it reads.
pub(crate) fn drawn_at(metadata: &str, created_at: &str) -> String {
    let parse = |raw: &str| chrono::DateTime::parse_from_rfc3339(raw.trim()).ok();
    serde_json::from_str::<serde_json::Value>(metadata)
        .ok()
        .and_then(|m| parse(m.get("capture_revision")?.as_str()?))
        .filter(|at| parse(created_at).is_some_and(|created| *at <= created))
        .map(|at| at.with_timezone(&chrono::Utc).to_rfc3339())
        .unwrap_or_else(|| created_at.to_string())
}

/// A fact's `source_capture_id` and [`drawn_at`] revision.
pub(crate) type FactStamp = (String, String);
/// A live, not-derived capture row's `capture_id` and `updated_at`.
pub(crate) type CaptureStamp = (String, String);

/// What the settled-capture count reads from a row: nothing but ids and
/// times, so no body, tags or revision is carried past this point.
pub(crate) enum Stamp {
    Fact(FactStamp),
    Capture(CaptureStamp),
}

/// The [`Stamp`] row `row` contributes to the settled-capture count, if any.
pub(crate) fn stamp(row: &MemoryRow) -> Option<Stamp> {
    if let Some(source) = &row.source_capture_id {
        return Some(Stamp::Fact((source.clone(), drawn_from(row))));
    }
    let capture = row
        .capture_id
        .as_ref()
        .filter(|_| row.deleted_at.is_none())?;
    Some(Stamp::Capture((capture.clone(), row.updated_at.clone())))
}

/// How many distinct captures [`undecomposed_rows_in`] lists rows for: the
/// same eligibility (live, not derived, needing decomposition, last changed
/// at or before `settled_before`), counted by `capture_id` without sorting
/// or holding rows.
pub(crate) fn count_settled_captures(
    facts: impl Iterator<Item = FactStamp>,
    captures: impl Iterator<Item = CaptureStamp>,
    settled_before: Option<&str>,
) -> usize {
    let mut decomposed: HashMap<String, String> = HashMap::new();
    for (capture, at) in facts {
        let slot = decomposed.entry(capture).or_default();
        if at > *slot {
            *slot = at;
        }
    }
    captures
        .filter(|(_, updated)| settled_before.is_none_or(|cutoff| updated.as_str() <= cutoff))
        .filter(|(capture, updated)| decomposed.get(capture).is_none_or(|at| updated > at))
        .map(|(capture, _)| capture)
        .collect::<HashSet<_>>()
        .len()
}

/// Up to which revision each capture has been decomposed: the newest
/// [`drawn_from`] among the memories naming it as their `source_capture_id`.
fn decomposed_at(rows: impl Iterator<Item = MemoryRow>) -> HashMap<String, String> {
    let mut newest: HashMap<String, String> = HashMap::new();
    for row in rows {
        let Some(capture) = row.source_capture_id.clone() else {
            continue;
        };
        let at = drawn_from(&row);
        let slot = newest.entry(capture).or_default();
        if at > *slot {
            *slot = at;
        }
    }
    newest
}

/// The newest `updated_at` among each capture's own rows.
pub(crate) fn capture_revisions(rows: impl Iterator<Item = MemoryRow>) -> HashMap<String, String> {
    let mut newest: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (Some(capture), None) = (row.capture_id, row.source_capture_id) else {
            continue;
        };
        let slot = newest.entry(capture).or_default();
        if row.updated_at > *slot {
            *slot = row.updated_at;
        }
    }
    newest
}

/// Whether capture row `row` still needs decomposing: no fact names its
/// capture, or the row changed after the revision its newest facts were
/// drawn from (a session recaptured after, or during, decomposition;
/// ADR-0029).
pub(crate) fn needs_decomposing(row: &MemoryRow, decomposed: &HashMap<String, String>) -> bool {
    row.capture_id
        .as_ref()
        .is_some_and(|c| decomposed.get(c).is_none_or(|at| row.updated_at > *at))
}

/// The captures still needing decomposition, as [`needs_decomposing`] says.
pub(crate) fn undecomposed_captures(core: &CoreTables) -> Vec<MemoryRow> {
    undecomposed_in(|| memories::rows(core))
}

/// [`undecomposed_captures`] over any backend's rows, read twice.
pub(crate) fn undecomposed_in<I: Iterator<Item = MemoryRow>>(
    rows: impl Fn() -> I,
) -> Vec<MemoryRow> {
    let decomposed = decomposed_at(rows());
    rows()
        .filter(|row| row.deleted_at.is_none() && row.source_capture_id.is_none())
        .filter(|row| needs_decomposing(row, &decomposed))
        .collect()
}

/// Captures, not themselves decomposed facts and not deleted, that need
/// decomposing and last changed at or before `settled_before`, newest first.
fn undecomposed_rows(core: &CoreTables, settled_before: Option<&str>) -> Vec<MemoryRow> {
    undecomposed_rows_in(|| memories::rows(core), settled_before)
}

/// [`undecomposed_rows`] over any backend's rows.
pub(crate) fn undecomposed_rows_in<I: Iterator<Item = MemoryRow>>(
    rows: impl Fn() -> I,
    settled_before: Option<&str>,
) -> Vec<MemoryRow> {
    let mut rows: Vec<MemoryRow> = undecomposed_in(rows)
        .into_iter()
        .filter(|row| settled_before.is_none_or(|cutoff| row.updated_at.as_str() <= cutoff))
        .collect();
    newest_first(&mut rows);
    rows
}

// --- normalization ----------------------------------------------------------

/// The ids memories name as `metadata.normalized_from` (text only, as
/// `json_extract(…) = m.id` compares).
fn normalized_ids(rows: impl Iterator<Item = MemoryRow>) -> HashSet<String> {
    rows.filter_map(|row| {
        let metadata: Value = serde_json::from_str(&row.metadata).ok()?;
        metadata
            .get("normalized_from")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
    .collect()
}

fn unnormalized_rows(core: &CoreTables, sources: &[&str]) -> Vec<MemoryRow> {
    unnormalized_in(|| memories::rows(core), sources)
}

/// [`unnormalized_rows`] over any backend's rows.
pub(crate) fn unnormalized_in<I: Iterator<Item = MemoryRow>>(
    rows: impl Fn() -> I,
    sources: &[&str],
) -> Vec<MemoryRow> {
    let normalized = normalized_ids(rows());
    let mut rows: Vec<MemoryRow> = rows()
        .filter(|row| live(row) && sources.contains(&row.source.as_str()))
        .filter(|row| !normalized.contains(&row.id))
        .collect();
    newest_first(&mut rows);
    rows
}

// --- contradictions ---------------------------------------------------------

/// `lower(trim(text))` as SQLite computes it: spaces trimmed, ASCII folded.
fn folded(text: &str) -> String {
    text.trim_matches(' ').to_ascii_lowercase()
}

/// Whether both rows have a subject and predicate and they match folded.
pub(crate) fn same_claim(a: &MemoryRow, b: &MemoryRow) -> bool {
    match (&a.subject, &a.predicate, &b.subject, &b.predicate) {
        (Some(sa), Some(pa), Some(sb), Some(pb)) => {
            folded(sa) == folded(sb) && folded(pa) == folded(pb)
        }
        _ => false,
    }
}

/// Whether both rows state an object and the two differ folded: the same
/// claim made twice with different answers, which supersession should have
/// settled and did not (a sync merge, an import). A restatement is not one.
fn conflicting_objects(a: &MemoryRow, b: &MemoryRow) -> bool {
    match (&a.object, &b.object) {
        (Some(oa), Some(ob)) => folded(oa) != folded(ob),
        _ => false,
    }
}

/// Every candidate pair `(id_a, id_b)` with `id_a < id_b`, in order.
fn pairs(core: &CoreTables, max_fanout: i64) -> BTreeSet<(String, String)> {
    pairs_in(
        super::graph::link_pairs(core),
        |id| memories::row(core, id),
        max_fanout,
    )
}

/// [`pairs`] over any backend's links `(memory id, entity id)` and rows.
pub(crate) fn pairs_in(
    links: Vec<(String, String)>,
    row: impl Fn(&str) -> Option<MemoryRow>,
    max_fanout: i64,
) -> BTreeSet<(String, String)> {
    let mut by_entity: HashMap<String, Vec<String>> = HashMap::new();
    for (memory_id, entity_id) in links {
        by_entity.entry(entity_id).or_default().push(memory_id);
    }
    let eligible = |id: &str| row(id).filter(|row| live(row) && row.category != "dialog");
    let mut found = BTreeSet::new();
    for ids in by_entity.values() {
        if i64::try_from(ids.len()).unwrap_or(i64::MAX) > max_fanout {
            continue;
        }
        for a in ids {
            for b in ids.iter().filter(|b| *b > a) {
                let (Some(ra), Some(rb)) = (eligible(a), eligible(b)) else {
                    continue;
                };
                if !same_claim(&ra, &rb) || conflicting_objects(&ra, &rb) {
                    found.insert((a.clone(), b.clone()));
                }
            }
        }
    }
    found
}

/// Whether `row` is a decision still in force: live, not sensitive, and not
/// resolved as reverted, abandoned or superseded. Sensitive rows are left
/// out because the result rides an ordinary write's response, which must not
/// surface them unasked.
pub(crate) fn standing_decision(row: &MemoryRow) -> bool {
    live(row)
        && !row.sensitive
        && row.memory_type == "decision"
        && !matches!(
            row.outcome.as_deref(),
            Some("reverted" | "abandoned" | "superseded")
        )
}

/// Whether two rows were written in the same project, ignoring ASCII case.
/// Two rows with no project are in the same (unnamed) one; a row with a
/// project and one without are not, so a claim in one repository is never
/// matched against a same-named claim in another.
pub(crate) fn same_project(a: &MemoryRow, b: &MemoryRow) -> bool {
    match (&a.project, &b.project) {
        (Some(pa), Some(pb)) => pa.eq_ignore_ascii_case(pb),
        (None, None) => true,
        _ => false,
    }
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl CurationStore for EngineTables {
    /// Every memory carrying `capture_id`, by category, ties by id.
    fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        let mut rows: Vec<MemoryRow> = memories::rows(core_ref(self)?)
            .filter(|row| row.capture_id.as_deref() == Some(capture_id))
            .collect();
        rows.sort_by(|a, b| (&a.category, &a.id).cmp(&(&b.category, &b.id)));
        Ok(rows.iter().map(MemoryRow::to_memory).collect())
    }

    /// The tags of the lowest-id memory carrying `capture_id`.
    fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        Ok(memories::rows(core_ref(self)?)
            .filter(|row| row.capture_id.as_deref() == Some(capture_id))
            .min_by(|a, b| a.id.cmp(&b.id))
            .map(|row| tags(&row)))
    }

    fn undecomposed(&self, limit: usize, settled_before: Option<&str>) -> Result<Vec<CaptureRow>> {
        let core = core_ref(self)?;
        let revisions = capture_revisions(memories::rows(core));
        Ok(undecomposed_rows(core, settled_before)
            .into_iter()
            .take(limit)
            .map(|row| CaptureRow {
                tags: tags(&row),
                revision: row
                    .capture_id
                    .as_ref()
                    .and_then(|c| revisions.get(c).cloned())
                    .unwrap_or_else(|| row.updated_at.clone()),
                capture_id: row.capture_id.unwrap_or_default(),
                id: row.id,
                content: row.content,
                category: row.category,
            })
            .collect())
    }

    fn count_undecomposed(&self, settled_before: Option<&str>) -> Result<i64> {
        Ok(count(
            undecomposed_rows(core_ref(self)?, settled_before).len(),
        ))
    }

    /// One pass over the rows for all three observations.
    ///
    /// Known gap: the engine store has no narrow read. Its records live in
    /// memory and `get` returns an owned clone of the whole record, so each
    /// row's content is still copied once per call before it is dropped.
    /// Nothing is sorted, collected whole or built into a `CaptureRow`. A
    /// projection would need a borrowing accessor in
    /// `rusty_multimodal_db_engine`; SQLite is the chosen backend (R4), so
    /// that is not added here.
    fn refine_observation(
        &self,
        settled_before: Option<&str>,
        run_category: &str,
    ) -> Result<RefineObservation> {
        let (mut facts, mut captures) = (Vec::new(), Vec::new());
        let mut last_capture_at: Option<String> = None;
        let mut last_run: Option<(String, String)> = None;
        for row in memories::rows(core_ref(self)?) {
            let live = row.deleted_at.is_none();
            if live
                && row.capture_id.is_some()
                && last_capture_at
                    .as_deref()
                    .is_none_or(|l| row.created_at.as_str() > l)
            {
                last_capture_at = Some(row.created_at.clone());
            }
            if live
                && row.category == run_category
                && last_run
                    .as_ref()
                    .is_none_or(|(id, at)| (&row.created_at, &row.id) > (at, id))
            {
                last_run = Some((row.id.clone(), row.created_at.clone()));
            }
            match stamp(&row) {
                Some(Stamp::Fact(f)) => facts.push(f),
                Some(Stamp::Capture(c)) => captures.push(c),
                None => {}
            }
        }
        Ok(RefineObservation {
            settled_captures: count(count_settled_captures(
                facts.into_iter(),
                captures.into_iter(),
                settled_before,
            )),
            last_capture_at,
            last_run,
        })
    }

    fn capture_activity(&self) -> Result<CaptureActivity> {
        let mut captures = HashSet::new();
        let mut last: Option<String> = None;
        for row in memories::rows(core_ref(self)?).filter(|row| row.deleted_at.is_none()) {
            let Some(capture) = row.capture_id else {
                continue;
            };
            captures.insert(capture);
            if last.as_deref().is_none_or(|l| row.created_at.as_str() > l) {
                last = Some(row.created_at);
            }
        }
        Ok(CaptureActivity {
            captures: count(captures.len()),
            last_capture_at: last,
        })
    }

    fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>> {
        Ok(unnormalized_rows(core_ref(self)?, sources)
            .into_iter()
            .take(limit)
            .map(|row| ImportRow {
                tags: tags(&row),
                metadata: serde_json::from_str(&row.metadata)
                    .unwrap_or_else(|_| serde_json::json!({})),
                id: row.id,
                content: row.content,
                category: row.category,
                source: row.source,
            })
            .collect())
    }

    fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        Ok(count(unnormalized_rows(core_ref(self)?, sources).len()))
    }

    fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        Ok(
            memories::row(core_ref(self)?, memory_id).map(|row| NormalizationSource {
                tags: tags(&row),
                doc_id: row.doc_id,
                chunk_index: row.chunk_index,
            }),
        )
    }

    fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        let core = core_ref(self)?;
        let depth = match backlog {
            Backlog::Undecomposed => undecomposed_rows(core, None).len(),
            Backlog::Unannotated => {
                let linked: HashSet<String> = super::graph::link_pairs(core)
                    .into_iter()
                    .map(|(memory_id, _)| memory_id)
                    .collect();
                memories::rows(core)
                    .filter(|row| live(row) && row.category != "dialog")
                    .filter(|row| row.subject.is_none() && row.predicate.is_none())
                    .filter(|row| row.object.is_none() && !linked.contains(&row.id))
                    .count()
            }
            Backlog::Unnormalized => {
                unnormalized_rows(core, &["document_import", "chat_import"]).len()
            }
            Backlog::Unclassified => memories::rows(core)
                .filter(|row| row.deleted_at.is_none() && row.memory_type == "unclassified")
                .count(),
        };
        Ok(count(depth))
    }

    fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64> {
        Ok(count(pairs(core_ref(self)?, max_fanout).len()))
    }

    fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        Ok(pairs(core_ref(self)?, max_fanout)
            .into_iter()
            .filter(|(a, b)| cursor.is_none_or(|(ca, cb)| (a.as_str(), b.as_str()) > (ca, cb)))
            .take(limit)
            .collect())
    }

    /// The standing decisions other than `memory_id`, in its project, that make
    /// the same claim as it, or mention an entity it mentions that at most
    /// `max_fanout` memories mention, in id order, each with the names of those
    /// shared entities (by name). Empty when `memory_id` is missing.
    ///
    /// One pass over the links finds the saved memory's entities; each entity's
    /// memories then come from the link index, and the claim scan runs only when
    /// the saved memory states a subject and predicate.
    fn decision_neighbours(
        &self,
        memory_id: &str,
        max_fanout: i64,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let core = core_ref(self)?;
        let Some(saved) = memories::row(core, memory_id) else {
            return Ok(Vec::new());
        };
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entity_id in super::graph::entities_of(core, memory_id) {
            let ids = super::graph::linked_memory_ids(core, &entity_id);
            if i64::try_from(ids.len()).unwrap_or(i64::MAX) > max_fanout {
                continue;
            }
            let Some(name) = super::graph::entity_name(core, &entity_id) else {
                continue;
            };
            for id in ids {
                found.entry(id).or_default().insert(name.clone());
            }
        }
        if saved.subject.is_some() && saved.predicate.is_some() {
            for row in memories::rows(core).filter(|row| same_claim(row, &saved)) {
                found.entry(row.id).or_default();
            }
        }
        found.remove(memory_id);
        Ok(found
            .into_iter()
            .filter(|(id, _)| {
                memories::row(core, id)
                    .is_some_and(|row| standing_decision(&row) && same_project(&row, &saved))
            })
            .map(|(id, names)| (id, names.into_iter().collect()))
            .collect())
    }

    /// The names of the stored entities both memories mention, by name.
    fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        let core = core_ref(self)?;
        let links = super::graph::link_pairs(core);
        let of = |id: &str| -> HashSet<String> {
            links
                .iter()
                .filter(|(m, _)| m == id)
                .map(|(_, e)| e.clone())
                .collect()
        };
        let (a, b) = (of(id_a), of(id_b));
        let names: BTreeSet<String> = a
            .intersection(&b)
            .filter_map(|e| super::graph::entity_name(core, e))
            .collect();
        Ok(names.into_iter().collect())
    }

    /// One side of a pair, its content cut to `snippet_chars` characters. A
    /// missing memory is [`StoreError::NotFound`], as the SQL's `query_row` is.
    fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        let row = memories::row(core_ref(self)?, memory_id).ok_or(StoreError::NotFound)?;
        Ok(ContradictionSide {
            content_snippet: row.content.chars().take(snippet_chars).collect(),
            memory_type: Some(row.memory_type),
            id: row.id,
            category: row.category,
            subject: row.subject,
            predicate: row.predicate,
            object: row.object,
            created_at: row.created_at,
        })
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;
    crate::db::curation::curation_conformance!(EngineTables::open_temporary().unwrap());
}
